// Copyright 2017 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! The `secio` protocol is a middleware that will encrypt and decrypt communications going
//! through a socket (or anything that implements `AsyncRead + AsyncWrite`).
//!
//! # Connection upgrade
//!
//! The `SecioConfig` struct implements the `ConnectionUpgrade` trait. You can apply it over a
//! `Transport` by using the `with_upgrade` method. The returned object will also implement
//! `Transport` and will automatically apply the secio protocol over any connection that is opened
//! through it.
//!
//! ```no_run
//! extern crate futures;
//! extern crate tokio_current_thread;
//! extern crate tokio_io;
//! extern crate libp2p_core;
//! extern crate libp2p_secio;
//! extern crate libp2p_tcp_transport;
//!
//! # fn main() {
//! use futures::Future;
//! use libp2p_secio::{SecioConfig, SecioKeyPair, SecioOutput};
//! use libp2p_core::{Multiaddr, Transport, upgrade};
//! use libp2p_tcp_transport::TcpConfig;
//! use tokio_io::io::write_all;
//!
//! let transport = TcpConfig::new()
//!     .with_upgrade({
//!         # let private_key = b"";
//!         //let private_key = include_bytes!("test-rsa-private-key.pk8");
//!         # let public_key = vec![];
//!         //let public_key = include_bytes!("test-rsa-public-key.der").to_vec();
//!         let upgrade = SecioConfig {
//!             // See the documentation of `SecioKeyPair`.
//!             key: SecioKeyPair::rsa_from_pkcs8(private_key, public_key).unwrap(),
//!         };
//!
//!         upgrade::map(upgrade, |out: SecioOutput<_>| out.stream)
//!     });
//!
//! let future = transport.dial("/ip4/127.0.0.1/tcp/12345".parse::<Multiaddr>().unwrap())
//!        .unwrap_or_else(|_| panic!("Unable to dial node"))
//!     .and_then(|(connection, _)| {
//!         // Sends "hello world" on the connection, will be encrypted.
//!         write_all(connection, "hello world")
//!     });
//!
//! tokio_current_thread::block_on_all(future).unwrap();
//! # }
//! ```
//!
//! # Manual usage
//!
//! > **Note**: You are encouraged to use `SecioConfig` as described above.
//!
//! You can add the `secio` layer over a socket by calling `SecioMiddleware::handshake()`. This
//! method will perform a handshake with the host, and return a future that corresponds to the
//! moment when the handshake succeeds or errored. On success, the future produces a
//! `SecioMiddleware` that implements `Sink` and `Stream` and can be used to send packets of data.
//!

extern crate aes_ctr;
#[cfg(feature = "secp256k1")]
extern crate asn1_der;
extern crate bytes;
extern crate futures;
extern crate libp2p_core;
#[macro_use]
extern crate log;
extern crate protobuf;
extern crate rand;
extern crate ring;
extern crate rw_stream_sink;
#[cfg(feature = "secp256k1")]
extern crate secp256k1;
extern crate tokio_io;
extern crate untrusted;

#[cfg(feature = "aes-all")]
#[macro_use]
extern crate lazy_static;
pub use self::error::SecioError;

#[cfg(feature = "secp256k1")]
use asn1_der::{traits::FromDerEncoded, traits::FromDerObject, DerObject};
use bytes::{Bytes, BytesMut};
use futures::stream::MapErr as StreamMapErr;
use futures::{Future, Poll, Sink, StartSend, Stream};
use libp2p_core::{PeerId, PublicKey};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, RSAKeyPair};
use rw_stream_sink::RwStreamSink;
use std::error::Error;
use std::io::{Error as IoError, ErrorKind as IoErrorKind};
use std::iter;
use std::sync::Arc;
use tokio_io::{AsyncRead, AsyncWrite};
use untrusted::Input;

mod algo_support;
mod codec;
mod error;
mod handshake;
mod structs_proto;
mod stream_cipher;

/// Implementation of the `ConnectionUpgrade` trait of `libp2p_core`. Automatically applies
/// secio on any connection.
#[derive(Clone)]
pub struct SecioConfig {
    /// Private and public keys of the local node.
    pub key: SecioKeyPair,
}

/// Private and public keys of the local node.
///
/// # Generating offline keys with OpenSSL
///
/// ## RSA
///
/// Generating the keys:
///
/// ```ignore
/// openssl genrsa -out private.pem 2048
/// openssl rsa -in private.pem -outform DER -pubout -out public.der
/// openssl pkcs8 -in private.pem -topk8 -nocrypt -out private.pk8
/// rm private.pem      # optional
/// ```
///
/// Loading the keys:
///
/// ```ignore
/// let key_pair = SecioKeyPair::rsa_from_pkcs8(include_bytes!("private.pk8"),
///                                                include_bytes!("public.der"));
/// ```
///
#[derive(Clone)]
pub struct SecioKeyPair {
    inner: SecioKeyPairInner,
}

impl SecioKeyPair {
    /// Builds a `SecioKeyPair` from a PKCS8 private key and public key.
    pub fn rsa_from_pkcs8<P>(
        private: &[u8],
        public: P,
    ) -> Result<SecioKeyPair, Box<Error + Send + Sync>>
    where
        P: Into<Vec<u8>>,
    {
        let private = RSAKeyPair::from_pkcs8(Input::from(&private[..])).map_err(Box::new)?;

        Ok(SecioKeyPair {
            inner: SecioKeyPairInner::Rsa {
                public: public.into(),
                private: Arc::new(private),
            },
        })
    }

    /// Builds a `SecioKeyPair` from a PKCS8 ED25519 private key.
    pub fn ed25519_from_pkcs8<K>(key: K) -> Result<SecioKeyPair, Box<Error + Send + Sync>>
    where
        K: AsRef<[u8]>,
    {
        let key_pair = Ed25519KeyPair::from_pkcs8(Input::from(key.as_ref())).map_err(Box::new)?;

        Ok(SecioKeyPair {
            inner: SecioKeyPairInner::Ed25519 {
                key_pair: Arc::new(key_pair),
            },
        })
    }

    /// Generates a new Ed25519 key pair and uses it.
    pub fn ed25519_generated() -> Result<SecioKeyPair, Box<Error + Send + Sync>> {
        let rng = SystemRandom::new();
        let gen = Ed25519KeyPair::generate_pkcs8(&rng).map_err(Box::new)?;
        Ok(SecioKeyPair::ed25519_from_pkcs8(&gen[..])
            .expect("failed to parse generated Ed25519 key"))
    }

    /// Builds a `SecioKeyPair` from a raw secp256k1 32 bytes private key.
    #[cfg(feature = "secp256k1")]
    pub fn secp256k1_raw_key<K>(key: K) -> Result<SecioKeyPair, Box<Error + Send + Sync>>
    where
        K: AsRef<[u8]>,
    {
        let secp = secp256k1::Secp256k1::with_caps(secp256k1::ContextFlag::None);
        let private = secp256k1::key::SecretKey::from_slice(&secp, key.as_ref())?;

        Ok(SecioKeyPair {
            inner: SecioKeyPairInner::Secp256k1 { private },
        })
    }

    /// Builds a `SecioKeyPair` from a secp256k1 private key in DER format.
    #[cfg(feature = "secp256k1")]
    pub fn secp256k1_from_der<K>(key: K) -> Result<SecioKeyPair, Box<Error + Send + Sync>>
    where
        K: AsRef<[u8]>,
    {
        // See ECPrivateKey in https://tools.ietf.org/html/rfc5915
        let obj: Vec<DerObject> =
            FromDerEncoded::with_der_encoded(key.as_ref()).map_err(|err| err.to_string())?;
        let priv_key_obj = obj.into_iter()
            .nth(1)
            .ok_or_else(|| "Not enough elements in DER".to_string())?;
        let private_key: Vec<u8> =
            FromDerObject::from_der_object(priv_key_obj).map_err(|err| err.to_string())?;
        SecioKeyPair::secp256k1_raw_key(&private_key)
    }

    /// Returns the public key corresponding to this key pair.
    pub fn to_public_key(&self) -> PublicKey {
        match self.inner {
            SecioKeyPairInner::Rsa { ref public, .. } => PublicKey::Rsa(public.clone()),
            SecioKeyPairInner::Ed25519 { ref key_pair } => {
                PublicKey::Ed25519(key_pair.public_key_bytes().to_vec())
            }
            #[cfg(feature = "secp256k1")]
            SecioKeyPairInner::Secp256k1 { ref private } => {
                let secp = secp256k1::Secp256k1::with_caps(secp256k1::ContextFlag::SignOnly);
                let pubkey = secp256k1::key::PublicKey::from_secret_key(&secp, private)
                    .expect("wrong secp256k1 private key ; type safety violated");
                PublicKey::Secp256k1(pubkey.serialize_vec(&secp, true).to_vec())
            }
        }
    }

    /// Builds a `PeerId` corresponding to the public key of this key pair.
    #[inline]
    pub fn to_peer_id(&self) -> PeerId {
        self.to_public_key().into_peer_id()
    }

    // TODO: method to save generated key on disk?
}

// Inner content of `SecioKeyPair`.
#[derive(Clone)]
enum SecioKeyPairInner {
    Rsa {
        public: Vec<u8>,
        // We use an `Arc` so that we can clone the enum.
        private: Arc<RSAKeyPair>,
    },
    Ed25519 {
        // We use an `Arc` so that we can clone the enum.
        key_pair: Arc<Ed25519KeyPair>,
    },
    #[cfg(feature = "secp256k1")]
    Secp256k1 { private: secp256k1::key::SecretKey },
}

/// Output of the secio protocol.
pub struct SecioOutput<S>
where
    S: AsyncRead + AsyncWrite,
{
    /// The encrypted stream.
    pub stream: RwStreamSink<StreamMapErr<SecioMiddleware<S>, fn(SecioError) -> IoError>>,
    /// The public key of the remote.
    pub remote_key: PublicKey,
    /// Ephemeral public key used during the negotiation.
    pub ephemeral_public_key: Vec<u8>,
}

impl<S, Maf> libp2p_core::ConnectionUpgrade<S, Maf> for SecioConfig
where
    S: AsyncRead + AsyncWrite + 'static, // TODO: 'static :(
    Maf: 'static,                        // TODO: 'static :(
{
    type Output = SecioOutput<S>;
    type MultiaddrFuture = Maf;
    type Future = Box<Future<Item = (Self::Output, Maf), Error = IoError>>;
    type NamesIter = iter::Once<(Bytes, ())>;
    type UpgradeIdentifier = ();

    #[inline]
    fn protocol_names(&self) -> Self::NamesIter {
        iter::once(("/secio/1.0.0".into(), ()))
    }

    #[inline]
    fn upgrade(
        self,
        incoming: S,
        _: (),
        _: libp2p_core::Endpoint,
        remote_addr: Maf,
    ) -> Self::Future {
        debug!("Starting secio upgrade");

        let fut = SecioMiddleware::handshake(incoming, self.key);
        let wrapped = fut.map(|(stream_sink, pubkey, ephemeral)| {
            let mapped = stream_sink.map_err(map_err as fn(_) -> _);
            SecioOutput {
                stream: RwStreamSink::new(mapped),
                remote_key: pubkey,
                ephemeral_public_key: ephemeral,
            }
        }).map_err(map_err);
        Box::new(wrapped.map(move |out| (out, remote_addr)))
    }
}

#[inline]
fn map_err(err: SecioError) -> IoError {
    debug!("error during secio handshake {:?}", err);
    IoError::new(IoErrorKind::InvalidData, err)
}

/// Wraps around an object that implements `AsyncRead` and `AsyncWrite`.
///
/// Implements `Sink` and `Stream` whose items are frames of data. Each frame is encoded
/// individually, so you are encouraged to group data in few frames if possible.
pub struct SecioMiddleware<S> {
    inner: codec::FullCodec<S>,
}

impl<S> SecioMiddleware<S>
where
    S: AsyncRead + AsyncWrite,
{
    /// Attempts to perform a handshake on the given socket.
    ///
    /// On success, produces a `SecioMiddleware` that can then be used to encode/decode
    /// communications, plus the public key of the remote, plus the ephemeral public key.
    pub fn handshake<'a>(
        socket: S,
        key_pair: SecioKeyPair,
    ) -> Box<Future<Item = (SecioMiddleware<S>, PublicKey, Vec<u8>), Error = SecioError> + 'a>
    where
        S: 'a,
    {
        let fut = handshake::handshake(socket, key_pair).map(|(inner, pubkey, ephemeral)| {
            let inner = SecioMiddleware { inner };
            (inner, pubkey, ephemeral)
        });

        Box::new(fut)
    }
}

impl<S> Sink for SecioMiddleware<S>
where
    S: AsyncRead + AsyncWrite,
{
    type SinkItem = BytesMut;
    type SinkError = IoError;

    #[inline]
    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.inner.start_send(item)
    }

    #[inline]
    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.inner.poll_complete()
    }
}

impl<S> Stream for SecioMiddleware<S>
where
    S: AsyncRead + AsyncWrite,
{
    type Item = Vec<u8>;
    type Error = SecioError;

    #[inline]
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.inner.poll()
    }
}
