use std::{net::SocketAddr, time::Duration};

use musli::{en::Encode, mode::DefaultMode, Decode};
use musli_descriptive::Encoding;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{lookup_host, TcpListener, TcpStream},
    select,
    time::sleep,
};

use crate::{type_hash, wait_for_ok, Error, MapAddError, Result};

const MUSLI_CONFIG: Encoding = musli_descriptive::encoding::DEFAULT;

/// Note: this is really only intended for self-contained Docker networks.
#[derive(Debug)]
pub struct NetMessenger {
    stream: TcpStream,
    // buffer whose capacity is kept around
    buf: Vec<u8>,
}

pub async fn wait_for_ok_lookup_host(
    num_retries: u64,
    delay: Duration,
    host: &str,
) -> Result<SocketAddr> {
    async fn f(host: &str) -> Result<SocketAddr> {
        match lookup_host(host).await {
            Ok(mut addrs) => {
                if let Some(addr) = addrs.next() {
                    Ok(addr)
                } else {
                    Err(Error::from("empty addrs"))
                }
            }
            Err(e) => Err(e).map_add_err(|| "wait_for_ok_lookup_host(.., host: {host})"),
        }
    }
    wait_for_ok(num_retries, delay, || f(host)).await
}

impl NetMessenger {
    /// Note: you should use `wait_for_ok_lookup_host` before calling this
    /// function if the host may not even be available yet.
    pub async fn connect(host: &str, timeout: Duration) -> Result<Self> {
        let socket_addr = lookup_host(host)
            .await
            .map_add_err(|| ())?
            .next()
            .map_add_err(|| ())?;
        // we use cancel safety here
        select! {
            tmp = TcpStream::connect(socket_addr) => {
                let stream = tmp.map_add_err(||())?;
                Ok(Self {stream, buf: vec![]})
            }
            _ = sleep(timeout) => {
                Err(Error::timeout())
            }
        }
    }

    /// Binds to and listens on `socket_addr`, and accepts a single connection
    /// to message with. Expects `expect` as the connecting `NetMessenger`.
    /// Cancels the bind and returns a timeout error if `timeout` is reached
    /// first.
    pub async fn listen_single_connect(host: &str, timeout: Duration) -> Result<Self> {
        let socket_addr = lookup_host(host)
            .await?
            .next()
            .map_add_err(|| "no socket addresses from lookup_host(host)")?;
        let listener = TcpListener::bind(socket_addr).await.map_add_err(|| ())?;

        //let tmp = listener.accept().await?;
        //let (stream, socket) = tmp;
        //Ok(Self {stream, socket})
        // we use the cancel safety of `tokio::net::TcpListener::accept
        select! {
            tmp = listener.accept() => {
                let (stream, _) = tmp.map_add_err(||())?;
                Ok(Self {stream, buf: vec![]})
            }
            _ = sleep(timeout) => {
                Err(Error::timeout())
            }
        }
    }

    /// Note: You should always use the turbofish to specify `T`, because it is
    /// otherwise possible to get an unexpected type because of `&` coercion.
    ///
    /// Note: The hash of `std::any::type_name` is sent and compared to
    /// dynamically check if the correct `send` and `recv` pair are being used.
    /// This may break if the `send` and `recv` are sending from different
    /// binaries compiled by different compiler versions (but at least it is a
    /// false positive).
    pub async fn send<T: ?Sized + Encode<DefaultMode>>(&mut self, msg: &T) -> Result<()> {
        match MUSLI_CONFIG.encode(&mut self.buf, msg) {
            Ok(()) => (),
            Err(e) => return Err(Error::boxed(Box::new(e))),
        };
        // TODO handle timeouts
        let id = type_hash::<T>();
        self.stream.write_all(&id).await.map_add_err(|| ())?;
        self.stream
            .write_u64_le(u64::try_from(self.buf.len())?)
            .await
            .map_add_err(|| ())?;
        self.stream.write_all(&self.buf).await.map_add_err(|| ())?;
        self.stream.flush().await.map_add_err(|| ())?;
        Ok(())
    }

    pub async fn recv<'de, T: ?Sized + Decode<'de, DefaultMode>>(&'de mut self) -> Result<T> {
        // TODO handle timeouts
        let expected_id = type_hash::<T>();
        let mut actual_id = [0u8; 32];
        self.stream
            .read_exact(&mut actual_id)
            .await
            .map_add_err(|| "NetMessenger::recv() could not read_exact")?;
        if expected_id != actual_id {
            return Err(Error::from(
                "NetMessenger::recv() incoming type did not match expected type",
            ))
        }
        let data_len = usize::try_from(self.stream.read_u64_le().await?)?;
        if data_len > self.buf.len() {
            self.buf.resize_with(data_len, || 0);
        }
        self.stream.read_exact(&mut self.buf[0..data_len]).await?;
        match MUSLI_CONFIG.decode(&self.buf[0..data_len]) {
            Ok(o) => Ok(o),
            Err(e) => Err(Error::boxed(Box::new(e))),
        }
    }
}
