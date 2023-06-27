use core::cell::RefCell;

use esp_mbedtls::Certificates;
use esp_mbedtls::ConnectedSession;
use esp_mbedtls::Mode;
use esp_mbedtls::Session;
use esp_mbedtls::TlsVersion;
use esp_wifi::wifi_interface::Socket;
use esp_wifi::wifi_interface::WifiStackError;

use embedded_io::blocking::{Read, Write};
pub use embedded_nal;
use embedded_nal::TcpClientStack;
use embedded_nal::{TcpError, TcpErrorKind};



pub struct SocketHandle {
    handle: usize,
}

pub struct WifiTcpClientStack<'s, 'n: 's, const MAX_SOCKETS: usize = 1> {
    sockets: [WrappedSocket<'s, 'n>; MAX_SOCKETS],
    in_use: [bool; MAX_SOCKETS],
}

impl<'s, 'n: 's, const MAX_SOCKETS: usize> WifiTcpClientStack<'s, 'n, MAX_SOCKETS> {
    pub fn new(sockets: [WrappedSocket<'s, 'n>; MAX_SOCKETS]) -> Self {
        Self {
            sockets,
            in_use: [false; MAX_SOCKETS],
        }
    }
}


#[derive(Debug, Copy, Clone)]
pub struct WifiStackErrorWrapper(pub WifiStackError);

impl TcpError for WifiStackErrorWrapper {
    fn kind(&self) -> TcpErrorKind {
        match &self.0 {
            WifiStackError::DeviceError(_) => TcpErrorKind::PipeClosed,
            WifiStackError::InitializationError(_) => TcpErrorKind::Other,
            WifiStackError::Unknown(_) => TcpErrorKind::Other,
            WifiStackError::MissingIp => TcpErrorKind::Other,
        }
    }
}

impl<'s, 'n: 's, const MAX_SOCKETS: usize> TcpClientStack
    for WifiTcpClientStack<'s, 'n, MAX_SOCKETS>
{
    type TcpSocket = SocketHandle;
    type Error = WifiStackErrorWrapper;

    fn socket(&mut self) -> Result<Self::TcpSocket, Self::Error> {
        let first_unused = self
            .in_use
            .iter()
            .enumerate()
            .find(|(_index, v)| !**v)
            .map(|v| v.0);

        if let Some(first_unused_index) = first_unused {
            self.in_use[first_unused_index] = true;
            Ok(SocketHandle {
                handle: first_unused_index,
            })
        } else {
            Err(WifiStackErrorWrapper(WifiStackError::Unknown(0)))
        }
    }

    fn connect(
        &mut self,
        socket: &mut Self::TcpSocket,
        remote: embedded_nal::SocketAddr,
    ) -> embedded_nal::nb::Result<(), Self::Error> {
        let socket = &mut self.sockets[socket.handle];
        let remote_ip = match remote.ip() {
            embedded_nal::IpAddr::V4(ip) => {
                let octets = ip.octets();
                smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(
                    octets[0], octets[1], octets[2], octets[3],
                ))
            }
            embedded_nal::IpAddr::V6(_) => unimplemented!(),
        };
        let remote_port = remote.port();

        match socket.open(remote_ip, remote_port) {
            Ok(()) => Ok(()),
            Err(_e) => Err(embedded_nal::nb::Error::WouldBlock),
        }
    }

    // fn is_connected(&mut self, socket: &Self::TcpSocket) -> Result<bool, Self::Error> {
    //     let socket = &self.sockets[socket.handle];
    //     Ok(socket.is_connected())
    // }

    fn send(
        &mut self,
        socket: &mut Self::TcpSocket,
        buffer: &[u8],
    ) -> embedded_nal::nb::Result<usize, Self::Error> {
        let socket = &mut self.sockets[socket.handle];
        match socket.write(buffer) {
            Ok(n) => Ok(n),
            Err(_e) => Err(embedded_nal::nb::Error::WouldBlock),
        }
    }

    fn receive(
        &mut self,
        socket: &mut Self::TcpSocket,
        buffer: &mut [u8],
    ) -> embedded_nal::nb::Result<usize, Self::Error> {
        let socket = &mut self.sockets[socket.handle];
        match socket.read(buffer) {
            Ok(n) => Ok(n),
            Err(_e) => Err(embedded_nal::nb::Error::WouldBlock),
        }
    }

    fn close(&mut self, socket: Self::TcpSocket) -> Result<(), Self::Error> {
        let wrapped_socket = &self.sockets[socket.handle];
        wrapped_socket.close();
        self.in_use[socket.handle] = false;
        Ok(())
    }
}

pub struct WrappedSocket<'s, 'n: 's> {
    socket: RefCell<Option<Socket<'s, 'n>>>,
    tls: RefCell<Option<ConnectedSession<Socket<'s, 'n>>>>,
    certificates: Certificates<'s>,
    server_name: &'static str,
}

impl<'s, 'n: 's> WrappedSocket<'s, 'n> {
    pub fn new(
        socket: Socket<'s, 'n>,
        server_name: &'static str,
        certificates: Certificates<'s>,
    ) -> Self {
        Self {
            socket: RefCell::new(Some(socket)),
            tls: RefCell::new(None),
            certificates,
            server_name,
        }
    }

    pub fn open(
        &mut self,
        address: smoltcp::wire::IpAddress,
        port: u16,
    ) -> Result<(), esp_wifi::wifi_interface::IoError> {
        let mut socket = self.socket.get_mut().take().unwrap();
        socket.open(address, port)?;

        let tls = Session::new(
            socket,
            self.server_name,
            Mode::Client,
            TlsVersion::Tls1_2,
            self.certificates.clone(),
        )
        .unwrap();

        let tls = tls.connect().unwrap();
        *self.tls.get_mut() = Some(tls);

        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        // there is no is_connected in tls session - could check and remember any erros in write / read
        true
    }

    pub fn close(&self) {
        // there is no close in tls session - could just drop the session
    }

    pub fn write(&mut self, buffer: &[u8]) -> Result<usize, esp_wifi::wifi_interface::IoError> {
        let mut tls_socket = self.tls.borrow_mut();
        let tls_socket = tls_socket.as_mut().unwrap();

        tls_socket
            .write(buffer)
            .map_err(|_e| esp_wifi::wifi_interface::IoError::SocketClosed)
    }

    pub fn read(&mut self, buffer: &mut [u8]) -> Result<usize, esp_wifi::wifi_interface::IoError> {
        let mut tls_socket = self.tls.borrow_mut();
        let tls_socket = tls_socket.as_mut().unwrap();

        tls_socket
            .read(buffer)
            .map_err(|_e| esp_wifi::wifi_interface::IoError::SocketClosed)
    }
}