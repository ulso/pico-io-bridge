use embassy_net::tcp::{Error, TcpSocket};

pub(crate) enum Frame<'a> {
    Text(&'a [u8]),
    Binary,
    Ping(&'a [u8]),
    Pong,
    Close,
}

async fn read_exact(socket: &mut TcpSocket<'_>, mut out: &mut [u8]) -> Result<bool, Error> {
    while !out.is_empty() {
        let read = socket.read(out).await?;
        if read == 0 {
            return Ok(false);
        }
        out = &mut out[read..];
    }
    Ok(true)
}

async fn write_all(socket: &mut TcpSocket<'_>, mut data: &[u8]) -> Result<(), Error> {
    while !data.is_empty() {
        let written = socket.write(data).await?;
        data = &data[written..];
    }
    Ok(())
}

pub(crate) async fn read_frame<'a>(
    socket: &mut TcpSocket<'_>,
    payload: &'a mut [u8],
) -> Result<Option<Frame<'a>>, Error> {
    let mut header = [0u8; 2];
    if !read_exact(socket, &mut header).await? {
        return Ok(None);
    }

    let fin = header[0] & 0x80 != 0;
    let reserved = header[0] & 0x70 != 0;
    let opcode = header[0] & 0x0f;
    let masked = header[1] & 0x80 != 0;
    if !fin || reserved || !masked {
        return Ok(None);
    }

    let mut payload_len = (header[1] & 0x7f) as usize;
    if payload_len == 126 {
        let mut extended = [0u8; 2];
        if !read_exact(socket, &mut extended).await? {
            return Ok(None);
        }
        payload_len = u16::from_be_bytes(extended) as usize;
    } else if payload_len == 127 {
        let mut extended = [0u8; 8];
        if !read_exact(socket, &mut extended).await? {
            return Ok(None);
        }
        let extended = u64::from_be_bytes(extended);
        if extended > usize::MAX as u64 {
            return Ok(None);
        }
        payload_len = extended as usize;
    }

    if payload_len > payload.len() || (opcode & 0x08 != 0 && payload_len > 125) {
        return Ok(None);
    }

    let mut mask = [0u8; 4];
    if !read_exact(socket, &mut mask).await?
        || !read_exact(socket, &mut payload[..payload_len]).await?
    {
        return Ok(None);
    }

    for (index, byte) in payload[..payload_len].iter_mut().enumerate() {
        *byte ^= mask[index % mask.len()];
    }

    let payload = &payload[..payload_len];
    Ok(match opcode {
        0x1 => Some(Frame::Text(payload)),
        0x2 => Some(Frame::Binary),
        0x8 => Some(Frame::Close),
        0x9 => Some(Frame::Ping(payload)),
        0xA => Some(Frame::Pong),
        _ => None,
    })
}

async fn send_frame(socket: &mut TcpSocket<'_>, opcode: u8, payload: &[u8]) -> Result<(), Error> {
    if payload.len() < 126 {
        write_all(socket, &[0x80 | opcode, payload.len() as u8]).await?;
    } else {
        let len = u16::try_from(payload.len()).map_err(|_| Error::ConnectionReset)?;
        write_all(socket, &[0x80 | opcode, 126, (len >> 8) as u8, len as u8]).await?;
    }
    write_all(socket, payload).await
}

pub(crate) async fn send_text(socket: &mut TcpSocket<'_>, payload: &[u8]) -> Result<(), Error> {
    send_frame(socket, 0x1, payload).await
}

#[cfg(feature = "board-adafruit-rp2040-usb-host")]
pub(crate) async fn send_binary(socket: &mut TcpSocket<'_>, payload: &[u8]) -> Result<(), Error> {
    send_frame(socket, 0x2, payload).await
}

pub(crate) async fn send_pong(socket: &mut TcpSocket<'_>, payload: &[u8]) -> Result<(), Error> {
    send_frame(socket, 0xA, payload).await
}

pub(crate) async fn send_close(socket: &mut TcpSocket<'_>) -> Result<(), Error> {
    send_frame(socket, 0x8, &[]).await
}
