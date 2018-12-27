use bytes::{Bytes, BytesMut};
use futures::sync::mpsc::{self, Receiver, Sender};
use futures::{prelude::*, sink::Sink};
use hmac;
use log::{debug, warn};
use tokio::codec::{length_delimited::LengthDelimitedCodec, Framed};
use tokio::prelude::{AsyncRead, AsyncWrite};

use std::cmp::min;
use std::collections::VecDeque;
use std::io;

use crate::{
    codec::{stream_handle::StreamEvent, stream_handle::StreamHandle, Hmac, StreamCipher},
    error::SecioError,
};

/// Encrypted stream
pub struct SecureStream<T> {
    socket: Framed<T, LengthDelimitedCodec>,
    dead: bool,

    cipher: StreamCipher,
    hmac: Hmac,
    /// denotes a sequence of bytes which are expected to be
    /// found at the beginning of the stream and are checked for equality
    nonce: Vec<u8>,

    pending: VecDeque<Bytes>,

    read_buf: VecDeque<BytesMut>,

    frame_sender: Option<Sender<StreamEvent>>,
    // For receive events from sub streams (for clone to stream handle)
    event_sender: Sender<StreamEvent>,
    // For receive events from sub streams
    event_receiver: Receiver<StreamEvent>,
}

impl<T> SecureStream<T>
where
    T: AsyncRead + AsyncWrite,
{
    /// New a secure stream
    pub fn new(
        socket: Framed<T, LengthDelimitedCodec>,
        cipher: StreamCipher,
        hmac: Hmac,
        nonce: Vec<u8>,
    ) -> Self {
        let (event_sender, event_receiver) = mpsc::channel(1024);
        SecureStream {
            socket,
            dead: false,
            cipher,
            hmac,
            read_buf: VecDeque::default(),
            nonce,
            pending: VecDeque::default(),
            frame_sender: None,
            event_sender,
            event_receiver,
        }
    }

    /// Create a unique handle to this stream.
    /// Repeated calls will return Error.
    pub fn create_handle(&mut self) -> Result<StreamHandle, ()> {
        if self.frame_sender.is_some() {
            return Err(());
        }
        let (frame_sender, frame_receiver) = mpsc::channel(1024);
        self.frame_sender = Some(frame_sender);
        Ok(StreamHandle::new(frame_receiver, self.event_sender.clone()))
    }

    fn handle_event(&mut self, event: StreamEvent) -> Result<(), io::Error> {
        match event {
            StreamEvent::Frame(mut frame) => {
                debug!("start send data: {:?}", frame);
                self.encode(&mut frame);
                self.pending.push_back(frame.freeze());
                while let Some(frame) = self.pending.pop_front() {
                    if let AsyncSink::NotReady(data) = self.socket.start_send(frame)? {
                        debug!("can't send");
                        self.pending.push_front(data);
                        break;
                    } else {
                        // TODO: not ready???
                        self.socket.poll_complete()?;
                    }
                }
            }
            StreamEvent::Close => {
                self.dead = true;
                let _ = self.socket.close();
            }
            StreamEvent::Flush(sender) => {
                let _ = self.socket.poll_complete();
                let _ = sender.send(());
                debug!("secure stream flushed");
            }
        }
        Ok(())
    }

    fn recv_frame(&mut self) -> Poll<Option<()>, SecioError> {
        loop {
            match self.socket.poll() {
                Ok(Async::Ready(Some(t))) => {
                    debug!("receive raw data: {:?}", t);
                    let data = self.decode(&t)?;
                    debug!("receive data: {:?}", data);
                    self.read_buf.push_back(BytesMut::from(data));
                    if let Some(ref mut sender) = self.frame_sender {
                        while let Some(data) = self.read_buf.pop_front() {
                            let _ = sender.try_send(StreamEvent::Frame(data));
                        }
                    }
                }
                Ok(Async::Ready(None)) => {
                    debug!("shutdown");
                    return Ok(Async::Ready(None));
                }
                Ok(Async::NotReady) => {
                    debug!("receive not ready");
                    return Ok(Async::NotReady);
                }
                Err(err) => {
                    return Err(err.into());
                }
            };
        }
    }

    /// Decoding data
    #[inline]
    fn decode(&mut self, frame: &BytesMut) -> Result<Vec<u8>, SecioError> {
        if frame.len() < self.hmac.num_bytes() {
            debug!("frame too short when decoding secio frame");
            return Err(SecioError::FrameTooShort);
        }

        let content_length = frame.len() - self.hmac.num_bytes();
        {
            let (crypted_data, expected_hash) = frame.split_at(content_length);
            debug_assert_eq!(expected_hash.len(), self.hmac.num_bytes());

            if self.hmac.verify(crypted_data, expected_hash).is_err() {
                debug!("hmac mismatch when decoding secio frame");
                return Err(SecioError::HmacNotMatching);
            }
        }

        let mut data_buf = frame.to_vec();
        data_buf.truncate(content_length);
        self.cipher.decrypt(&mut data_buf);

        if !self.nonce.is_empty() {
            let n = min(data_buf.len(), self.nonce.len());
            if data_buf[..n] != self.nonce[..n] {
                return Err(SecioError::NonceVerificationFailed);
            }
            self.nonce.drain(..n);
            data_buf.drain(..n);
        }
        Ok(data_buf)
    }

    /// Encoding data
    #[inline]
    fn encode(&mut self, data: &mut BytesMut) {
        self.cipher.encrypt(&mut data[..]);
        let signature = self.hmac.sign(&data[..]);
        data.extend_from_slice(signature.as_ref());
    }
}

impl<T> Stream for SecureStream<T>
where
    T: AsyncRead + AsyncWrite,
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if self.dead {
            return Ok(Async::Ready(None));
        }

        match self.recv_frame() {
            Ok(Async::Ready(None)) => {
                if let Some(mut sender) = self.frame_sender.take() {
                    let _ = sender.try_send(StreamEvent::Close);
                    return Ok(Async::Ready(None));
                }
            }
            Err(err) => {
                // todo: There was a problem during the run, how to do it?
                warn!("receive frame error: {:?}", err);
            }
            _ => (),
        }

        loop {
            match self.event_receiver.poll() {
                Ok(Async::Ready(Some(event))) => {
                    if let Err(err) = self.handle_event(event) {
                        warn!("send message error: {:?}", err);
                        break;
                    }
                }
                Ok(Async::Ready(None)) => unreachable!(),
                Ok(Async::NotReady) => break,
                Err(err) => {
                    warn!("receive event error: {:?}", err);
                    break;
                }
            }
        }

        Ok(Async::NotReady)
    }
}

#[cfg(test)]
mod tests {
    use super::{Hmac, SecureStream};
    use crate::stream_cipher::{ctr_int, Cipher};
    use crate::Digest;
    use bytes::BytesMut;
    use futures::{sync, Future, Stream};
    use rand;
    use std::io::Write;
    use std::thread;
    use tokio::codec::{length_delimited::LengthDelimitedCodec, Framed};
    use tokio::net::{TcpListener, TcpStream};

    const NULL_IV: [u8; 16] = [0; 16];

    #[test]
    fn test_decode_encode() {
        let cipher_key: [u8; 32] = rand::random();
        let hmac_key: [u8; 32] = rand::random();

        let data = b"hello world";

        let mut encode_data = BytesMut::from(data.to_vec());

        let mut encode_cipher = ctr_int(Cipher::Aes256, &cipher_key, &NULL_IV);
        let mut encode_hmac = Hmac::from_key(Digest::Sha256, &hmac_key);
        let mut decode_cipher = ctr_int(Cipher::Aes256, &cipher_key, &NULL_IV);
        let mut decode_hmac = encode_hmac.clone();

        encode_cipher.encrypt(&mut encode_data[..]);
        let signature = encode_hmac.sign(&encode_data[..]);
        encode_data.extend_from_slice(signature.as_ref());

        let content_length = encode_data.len() - decode_hmac.num_bytes();

        let (crypted_data, expected_hash) = encode_data.split_at(content_length);

        assert!(decode_hmac.verify(crypted_data, expected_hash).is_ok());

        let mut decode_data = encode_data.to_vec();
        decode_data.truncate(content_length);
        decode_cipher.decrypt(&mut decode_data);

        assert_eq!(&decode_data[..], &data[..]);
    }

    fn secure_codec_encode_then_decode(cipher: Cipher) {
        let cipher_key: [u8; 32] = rand::random();
        let cipher_key_clone = cipher_key.clone();
        let key_size = cipher.key_size();
        let hmac_key: [u8; 16] = rand::random();
        let hmac_key_clone = hmac_key.clone();
        let data = b"hello world";
        let data_clone = data.clone();
        let nonce = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9];

        let listener = TcpListener::bind(&"127.0.0.1:0".parse().unwrap()).unwrap();
        let listener_addr = listener.local_addr().unwrap();

        let (sender, receiver) = sync::oneshot::channel::<bytes::BytesMut>();

        let nonce2 = nonce.clone();
        let server = listener
            .incoming()
            .into_future()
            .map_err(|_| ())
            .map(move |(socket, _)| {
                let nonce2 = nonce2.clone();
                let mut secure = SecureStream::new(
                    Framed::new(socket.unwrap(), LengthDelimitedCodec::new()),
                    ctr_int(cipher, &cipher_key_clone[..key_size], &NULL_IV[..]),
                    Hmac::from_key(Digest::Sha256, &hmac_key_clone),
                    nonce2,
                );
                let handle = secure.create_handle().unwrap();

                let task = tokio::io::read_exact(handle, [0u8; 11])
                    .and_then(move |(_, data)| {
                        let _ = sender.send(BytesMut::from(data.to_vec()));
                        Ok(())
                    })
                    .map_err(|_| ());

                tokio::spawn(secure.for_each(|_| Ok(())));
                tokio::spawn(task);
                ()
            });

        let client = TcpStream::connect(&listener_addr)
            .map(move |stream| {
                let mut secure = SecureStream::new(
                    Framed::new(stream, LengthDelimitedCodec::new()),
                    ctr_int(cipher, &cipher_key_clone[..key_size], &NULL_IV[..]),
                    Hmac::from_key(Digest::Sha256, &hmac_key_clone),
                    Vec::new(),
                );
                let mut handle = secure.create_handle().unwrap();
                let _ = handle.write_all(&nonce);
                let _ = handle.write_all(&data_clone[..]);

                tokio::spawn(secure.for_each(|_| Ok(())));
                ()
            })
            .map_err(|_| ());

        thread::spawn(|| {
            tokio::run(server);
        });

        thread::spawn(|| {
            tokio::run(client);
        });

        let received = receiver.wait().unwrap();
        assert_eq!(received.to_vec(), data);
    }

    #[test]
    fn secure_codec_encode_then_decode_aes128() {
        secure_codec_encode_then_decode(Cipher::Aes128);
    }

    #[test]
    fn secure_codec_encode_then_decode_aes256() {
        secure_codec_encode_then_decode(Cipher::Aes256);
    }

    #[test]
    fn secure_codec_encode_then_decode_twofish() {
        secure_codec_encode_then_decode(Cipher::TwofishCtr);
    }
}
