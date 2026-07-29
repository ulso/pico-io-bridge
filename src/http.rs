use core::fmt::Write;

#[cfg(feature = "can")]
use embassy_futures::select::Either;
use embassy_futures::select::select;
use embassy_net::tcp::TcpSocket;
use embassy_time::{Duration, Timer};
use heapless::String;

#[cfg(feature = "can")]
use crate::can::{CAN_EVENTS, CanEvent, handle_can_ws_text, write_can_frame_json};
#[cfg(feature = "i2c")]
use crate::i2c::handle_i2c_ws_text;
#[cfg(any(feature = "can", feature = "i2c"))]
use crate::websocket::{self, Frame};

const HTTP_SOCKETS: usize = crate::HTTP_SOCKETS;
const HTTP_PEER_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

enum WebSocketEndpoint {
    #[cfg(feature = "can")]
    Can,
    #[cfg(feature = "i2c")]
    I2c,
}

#[cfg(feature = "board-adafruit-rp2040-usb-host")]
const API_STATUS_CAPABILITIES: &str = r#","interfaces":["usb-host","i2c","scpi"],"websocket":"/i2c","websockets":["/i2c"],"pages":{"i2c":"/","scpi":"/scpi.html"}"#;
#[cfg(all(
    not(feature = "board-adafruit-rp2040-usb-host"),
    feature = "can",
    feature = "i2c"
))]
const API_STATUS_CAPABILITIES: &str = r#","interfaces":["can","i2c","scpi"],"websocket":"/can","websockets":["/can","/i2c"],"pages":{"can":"/","i2c":"/i2c.html","scpi":"/scpi.html"}"#;
#[cfg(all(
    not(feature = "board-adafruit-rp2040-usb-host"),
    feature = "can",
    not(feature = "i2c")
))]
const API_STATUS_CAPABILITIES: &str = r#","interfaces":["can","scpi"],"websocket":"/can","websockets":["/can"],"pages":{"can":"/","scpi":"/scpi.html"}"#;
#[cfg(all(
    not(feature = "board-adafruit-rp2040-usb-host"),
    not(feature = "can"),
    feature = "i2c"
))]
const API_STATUS_CAPABILITIES: &str = r#","interfaces":["i2c","scpi"],"websocket":"/i2c","websockets":["/i2c"],"pages":{"i2c":"/","scpi":"/scpi.html"}"#;
#[cfg(all(
    not(feature = "board-adafruit-rp2040-usb-host"),
    not(any(feature = "can", feature = "i2c"))
))]
const API_STATUS_CAPABILITIES: &str =
    r#","interfaces":["scpi"],"websockets":[],"pages":{"scpi":"/scpi.html"}"#;

fn websocket_endpoint(request: &str) -> Option<WebSocketEndpoint> {
    #[cfg(feature = "can")]
    if request.starts_with("GET /can ") || request.starts_with("GET /ws ") {
        return Some(WebSocketEndpoint::Can);
    }

    #[cfg(feature = "i2c")]
    if request.starts_with("GET /i2c ") {
        return Some(WebSocketEndpoint::I2c);
    }

    let _ = request;
    None
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h0 = 0x6745_2301u32;
    let mut h1 = 0xefcd_ab89u32;
    let mut h2 = 0x98ba_dcfeu32;
    let mut h3 = 0x1032_5476u32;
    let mut h4 = 0xc3d2_e1f0u32;
    let bit_len = (data.len() as u64) * 8;
    let mut offset = 0;

    while offset + 64 <= data.len() {
        sha1_block(
            &data[offset..offset + 64],
            &mut h0,
            &mut h1,
            &mut h2,
            &mut h3,
            &mut h4,
        );
        offset += 64;
    }

    let mut block = [0u8; 128];
    let rem = &data[offset..];
    block[..rem.len()].copy_from_slice(rem);
    block[rem.len()] = 0x80;
    let total = if rem.len() + 1 + 8 <= 64 { 64 } else { 128 };
    block[total - 8..total].copy_from_slice(&bit_len.to_be_bytes());

    sha1_block(&block[..64], &mut h0, &mut h1, &mut h2, &mut h3, &mut h4);
    if total == 128 {
        sha1_block(&block[64..128], &mut h0, &mut h1, &mut h2, &mut h3, &mut h4);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

fn sha1_block(block: &[u8], h0: &mut u32, h1: &mut u32, h2: &mut u32, h3: &mut u32, h4: &mut u32) {
    let mut w = [0u32; 80];

    for (i, word) in w.iter_mut().take(16).enumerate() {
        let j = i * 4;
        *word = u32::from_be_bytes([block[j], block[j + 1], block[j + 2], block[j + 3]]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }

    let mut a = *h0;
    let mut b = *h1;
    let mut c = *h2;
    let mut d = *h3;
    let mut e = *h4;

    for (i, word) in w.iter().enumerate() {
        let (f, k) = match i {
            0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999),
            20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
            _ => (b ^ c ^ d, 0xca62_c1d6),
        };
        let temp = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(*word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = temp;
    }

    *h0 = h0.wrapping_add(a);
    *h1 = h1.wrapping_add(b);
    *h2 = h2.wrapping_add(c);
    *h3 = h3.wrapping_add(d);
    *h4 = h4.wrapping_add(e);
}

fn base64_20(input: &[u8; 20], out: &mut [u8; 28]) {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut i = 0;
    let mut j = 0;

    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | input[i + 2] as u32;
        out[j] = B64[((n >> 18) & 0x3f) as usize];
        out[j + 1] = B64[((n >> 12) & 0x3f) as usize];
        out[j + 2] = B64[((n >> 6) & 0x3f) as usize];
        out[j + 3] = B64[(n & 0x3f) as usize];
        i += 3;
        j += 4;
    }

    let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
    out[j] = B64[((n >> 18) & 0x3f) as usize];
    out[j + 1] = B64[((n >> 12) & 0x3f) as usize];
    out[j + 2] = B64[((n >> 6) & 0x3f) as usize];
    out[j + 3] = b'=';
}

fn header_value<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    for line in request.lines() {
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case(name)
        {
            return Some(value.trim());
        }
    }

    None
}

fn websocket_accept_key(key: &str, out: &mut [u8; 28]) {
    const GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let mut input = [0u8; 96];
    let key_bytes = key.as_bytes();
    let key_len = key_bytes.len().min(input.len() - GUID.len());

    input[..key_len].copy_from_slice(&key_bytes[..key_len]);
    input[key_len..key_len + GUID.len()].copy_from_slice(GUID);

    let digest = sha1(&input[..key_len + GUID.len()]);
    base64_20(&digest, out);
}

async fn write_all(
    socket: &mut TcpSocket<'_>,
    mut data: &[u8],
) -> Result<(), embassy_net::tcp::Error> {
    while !data.is_empty() {
        let written = socket.write(data).await?;
        data = &data[written..];
    }

    Ok(())
}

async fn write_http_response(
    socket: &mut TcpSocket<'_>,
    content_type: &str,
    body: &[u8],
) -> Result<(), embassy_net::tcp::Error> {
    let mut header = String::<160>::new();
    let _ = core::write!(
        header,
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        content_type,
        body.len()
    );
    write_all(socket, header.as_bytes()).await?;
    write_all(socket, body).await
}

async fn write_api_status_response(
    socket: &mut TcpSocket<'_>,
    serial: &str,
) -> Result<(), embassy_net::tcp::Error> {
    let mut body = String::<640>::new();
    write!(
        body,
        "{{\"device\":\"pico-io-bridge\",\"manufacturer\":\"{}\",\"board\":\"{}\",\"serial\":\"{}\",\"firmware\":\"{}\",\"network\":\"cdc-ncm\",\"scpi\":{{\"protocol\":\"SCPI-RAW\",\"port\":{},\"service\":\"_scpi-raw._tcp\"}}{}}}",
        crate::MANUFACTURER,
        crate::board::BOARD_NAME,
        serial,
        crate::FIRMWARE_VERSION,
        crate::SCPI_PORT,
        API_STATUS_CAPABILITIES
    )
    .unwrap();
    write_http_response(socket, "application/json", body.as_bytes()).await
}

async fn write_empty_response(
    socket: &mut TcpSocket<'_>,
    status: &str,
) -> Result<(), embassy_net::tcp::Error> {
    let mut header = String::<128>::new();
    let _ = core::write!(
        header,
        "HTTP/1.1 {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        status
    );
    write_all(socket, header.as_bytes()).await
}

async fn write_not_found(socket: &mut TcpSocket<'_>) -> Result<(), embassy_net::tcp::Error> {
    const BODY: &[u8] = b"not found\n";
    write_all(
        socket,
        b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: 10\r\n\r\n",
    )
    .await?;
    write_all(socket, BODY).await
}

async fn close_gracefully(socket: &mut TcpSocket<'_>) {
    // Send and acknowledge the response body before starting the TCP close handshake.
    if socket.flush().await.is_err() {
        socket.abort();
        let _ = socket.flush().await;
        return;
    }

    socket.close();
    if socket.flush().await.is_err() {
        socket.abort();
        let _ = socket.flush().await;
        return;
    }

    // close() only closes our transmit half. Give the browser time to send its FIN so
    // the socket reaches TIME_WAIT instead of being reset when the accept loop reuses it.
    let mut discard = [0u8; 64];
    let peer_close = async {
        loop {
            match socket.read(&mut discard).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    };
    let _ = select(peer_close, Timer::after(HTTP_PEER_CLOSE_TIMEOUT)).await;
}

async fn serve_http_connection(
    socket: &mut TcpSocket<'_>,
    rx_buf: &mut [u8],
    serial: &str,
) -> Result<(), embassy_net::tcp::Error> {
    let mut len = 0;

    loop {
        let n = socket.read(&mut rx_buf[len..]).await?;
        if n == 0 {
            return Ok(());
        }
        len += n;

        if rx_buf[..len].windows(4).any(|w| w == b"\r\n\r\n") || len == rx_buf.len() {
            break;
        }
    }

    let Ok(request) = core::str::from_utf8(&rx_buf[..len]) else {
        write_all(
            socket,
            b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        )
        .await?;
        return Ok(());
    };

    #[cfg(feature = "i2c")]
    {
        if request.starts_with("GET /i2c.html ") {
            const BODY: &[u8] = include_bytes!("i2c.html");
            write_http_response(socket, "text/html", BODY).await?;
            return Ok(());
        }
    }

    if request.starts_with("GET /scpi.html ") {
        const BODY: &[u8] = include_bytes!("scpi.html");
        write_http_response(socket, "text/html", BODY).await?;
        return Ok(());
    }

    let websocket_endpoint = websocket_endpoint(request);

    if let Some(endpoint) = websocket_endpoint {
        if let Some(key) = header_value(request, "Sec-WebSocket-Key") {
            let mut accept = [0u8; 28];
            websocket_accept_key(key, &mut accept);

            write_all(socket, b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ").await?;
            write_all(socket, &accept).await?;
            write_all(socket, b"\r\n\r\n").await?;
            socket.set_timeout(Some(crate::WS_TIMEOUT));
            socket.set_keep_alive(Some(crate::WS_KEEPALIVE));
            match endpoint {
                #[cfg(feature = "can")]
                WebSocketEndpoint::Can => can_websocket_loop(socket, rx_buf).await?,
                #[cfg(feature = "i2c")]
                WebSocketEndpoint::I2c => i2c_websocket_loop(socket, rx_buf).await?,
            }
        } else {
            write_empty_response(socket, "400 Bad Request").await?;
        }
    } else if request.starts_with("GET /api/status ") {
        write_api_status_response(socket, serial).await?;
    } else if request.starts_with("GET /favicon.ico ") {
        write_all(
            socket,
            b"HTTP/1.1 204 No Content\r\nConnection: close\r\nContent-Length: 0\r\nCache-Control: max-age=86400\r\n\r\n",
        )
        .await?;
    } else if request.starts_with("GET / ") || request.starts_with("GET /index.html ") {
        #[cfg(feature = "can")]
        {
            const BODY: &[u8] = br#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Pico I/O Bridge</title>
<link rel="icon" href="data:,">
<style>
:root{color-scheme:light dark;font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}
body{margin:0;background:#f3f5f7;color:#17202a}
main{max-width:1120px;margin:0 auto;padding:20px}
header{display:flex;align-items:center;justify-content:space-between;gap:16px;margin-bottom:14px}
h1{font-size:24px;line-height:1.2;margin:0;font-weight:700}
.status{display:inline-flex;align-items:center;gap:8px;font-size:14px;font-weight:650}
.dot{width:10px;height:10px;border-radius:50%;background:#b7c0ca}
.ok .dot{background:#1f9d55}.bad .dot{background:#d64545}.wait .dot{background:#d99a22}
.topbar{display:flex;align-items:center;justify-content:space-between;gap:12px;margin-bottom:14px}
.toolbar{display:flex;flex-wrap:wrap;gap:8px}
button{border:1px solid #9aa7b2;background:#fff;color:#17202a;border-radius:6px;padding:8px 12px;font:inherit;font-weight:650;min-height:38px}
button:disabled{opacity:.45}.primary{background:#17202a;color:#fff;border-color:#17202a}
.navlink{display:inline-flex;align-items:center;border:1px solid #9aa7b2;background:#fff;color:#17202a;border-radius:6px;padding:7px 12px;text-decoration:none;font-weight:650}.navlink.active{background:#17202a;color:#fff;border-color:#17202a}.interfaceNav{display:contents}
.grid{display:grid;grid-template-columns:minmax(300px,360px) 1fr;gap:16px;align-items:start}
.stack{display:grid;gap:16px}
section{margin-top:16px}.panel{border:1px solid #c7d0da;background:#fff;border-radius:8px;padding:14px}
h2{font-size:15px;margin:0 0 12px;font-weight:700}
.metrics{display:grid;grid-template-columns:repeat(auto-fit,minmax(120px,1fr));gap:8px}
.metric{border:1px solid #d7dee6;border-radius:6px;padding:8px;background:#f9fafb}
.metric span{display:block;font-size:11px;color:#5d6975}.metric strong{font-size:18px}
.stateOk{color:#1f9d55}.stateWarn{color:#b7791f}.stateBad{color:#d64545}.stateOff{color:#65717d}
label{display:block;font-size:12px;font-weight:700;margin:0 0 5px;color:#303b46}
.form{display:grid;grid-template-columns:1fr 1fr;gap:10px}.wide{grid-column:1/-1}
input,select{box-sizing:border-box;width:100%;border:1px solid #bdc7d1;border-radius:6px;background:#fff;color:#17202a;padding:8px;font:14px ui-monospace,SFMono-Regular,Menlo,monospace}
.checks{display:flex;align-items:center;gap:16px;margin-top:2px}.checks label{display:flex;gap:6px;align-items:center;margin:0;font-weight:650}
input[type=checkbox]{width:auto}.hint{font-size:12px;color:#65717d;margin-top:6px}
.filters{display:flex;flex-wrap:wrap;align-items:end;gap:10px;margin-bottom:8px}
.filters label{margin:0}.filters .idFilter{min-width:150px;max-width:220px}.filters .check{display:flex;gap:6px;align-items:center;font-weight:650}
.tableWrap{border:1px solid #c7d0da;border-radius:8px;overflow:auto;background:#fff;max-height:430px}
table{width:100%;border-collapse:collapse;font:12px ui-monospace,SFMono-Regular,Menlo,monospace}
th,td{padding:7px 8px;border-bottom:1px solid #edf0f3;text-align:left;white-space:nowrap}
th{position:sticky;top:0;background:#f9fafb;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;font-weight:700;color:#303b46}
.dirRx{color:#1f7a4d;font-weight:700}.dirTx{color:#285ea8;font-weight:700}.dirErr{color:#b42318;font-weight:700}
pre{box-sizing:border-box;width:100%;min-height:120px;max-height:220px;overflow:auto;border:1px solid #c7d0da;border-radius:8px;background:#fff;color:#17202a;padding:10px;margin:0;font:12px ui-monospace,SFMono-Regular,Menlo,monospace;white-space:pre-wrap}
@media(max-width:760px){main{padding:14px}.grid{grid-template-columns:1fr}.metrics{grid-template-columns:repeat(2,1fr)}header,.topbar{align-items:flex-start;flex-direction:column}.form{grid-template-columns:1fr}}
@media(prefers-color-scheme:dark){body{background:#111820;color:#edf2f7}.panel,.tableWrap,pre,input,select,button,.navlink{background:#17212b;color:#edf2f7;border-color:#3b4b5c}.metric,th{background:#1d2a36;border-color:#3b4b5c}.primary,.navlink.active{background:#edf2f7;color:#111820}.hint,.metric span,label{color:#b9c3cf}td,th{border-bottom-color:#273544}}
</style>
</head>
<body>
<main>
<header>
<h1 id="pageTitle">Pico I/O Bridge</h1>
<div id="status" class="status bad"><span class="dot"></span><span id="statusText">disconnected</span></div>
</header>
<div class="topbar">
<div class="toolbar">
<button id="connect">Connect</button>
<button id="disconnect" disabled>Disconnect</button>
<button id="clear">Clear</button>
<span id="interfaceNav" class="interfaceNav"><a class="navlink active" href="/">CAN</a></span>
</div>
<div class="status"><span id="bitrateLabel">CAN -</span><span id="modeLabel">mode -</span></div>
</div>
<section class="metrics">
<div class="metric"><span>RX frames</span><strong id="rxCount">0</strong></div>
<div class="metric"><span>TX requests</span><strong id="txCount">0</strong></div>
<div class="metric"><span>CAN state</span><strong id="canState">-</strong></div>
<div class="metric"><span>TX errors</span><strong id="txErrCount">0</strong></div>
<div class="metric"><span>RX errors</span><strong id="rxErrCount">0</strong></div>
<div class="metric"><span>TX queue</span><strong id="txQueue">-</strong></div>
<div class="metric"><span>RX queue</span><strong id="rxQueue">-</strong></div>
<div class="metric"><span>Last ID</span><strong id="lastId">-</strong></div>
</section>
<section class="grid">
<div class="stack">
<div class="panel">
<h2>Transmit</h2>
<div class="form">
<div><label for="canId">ID</label><input id="canId" value="123" inputmode="text"></div>
<div><label for="dlc">DLC</label><input id="dlc" value="2" inputmode="numeric"></div>
<div class="wide"><label for="data">Data bytes (hex)</label><input id="data" value="01 02" inputmode="text"><div class="hint">Hex bytes separated by spaces or commas</div></div>
<div class="wide checks"><label><input id="ext" type="checkbox"> Extended ID</label><label><input id="rtr" type="checkbox"> RTR</label></div>
<div class="wide toolbar"><button id="send" class="primary" disabled>Send Frame</button><button id="copyJson">Copy JSON</button></div>
</div>
</div>
<div class="panel">
<h2>Configuration</h2>
<div class="form">
<div><label for="bitrate">Bitrate</label><input id="bitrate" value="500000" inputmode="numeric"></div>
<div><label for="mode">Mode</label><select id="mode"><option value="normal">normal</option><option value="loopback">loopback</option><option value="listen-only">listen-only</option></select></div>
<div class="wide toolbar"><button id="applyConfig" disabled>Apply</button><button id="refreshStatus" disabled>Status</button></div>
<div class="wide hint"><span id="errorLabel">TX err 0 / RX err 0</span></div>
</div>
</div>
</div>
<div>
<div class="filters">
<label class="check"><input id="showRx" type="checkbox" checked> RX</label>
<label class="check"><input id="showTx" type="checkbox" checked> TX</label>
<div class="idFilter"><label for="filterId">Filter ID</label><input id="filterId" placeholder="123 or 0x123" inputmode="text"></div>
<button id="clearFrames">Clear Frames</button>
</div>
<div class="tableWrap">
<table><thead><tr><th>Time</th><th>Dir</th><th>ID</th><th>Fmt</th><th>DLC</th><th>Data</th></tr></thead><tbody id="frames"></tbody></table>
</div>
</div>
</section>
<section>
<pre id="log"></pre>
</section>
</main>
<script>
const statusEl=document.getElementById("status");
const statusText=document.getElementById("statusText");
const logEl=document.getElementById("log");
const connectBtn=document.getElementById("connect");
const disconnectBtn=document.getElementById("disconnect");
const sendBtn=document.getElementById("send");
const copyJsonBtn=document.getElementById("copyJson");
const applyConfigBtn=document.getElementById("applyConfig");
const refreshStatusBtn=document.getElementById("refreshStatus");
const clearBtn=document.getElementById("clear");
const framesEl=document.getElementById("frames");
const rxCountEl=document.getElementById("rxCount");
const txCountEl=document.getElementById("txCount");
const canStateEl=document.getElementById("canState");
const txErrCountEl=document.getElementById("txErrCount");
const rxErrCountEl=document.getElementById("rxErrCount");
const txQueueEl=document.getElementById("txQueue");
const rxQueueEl=document.getElementById("rxQueue");
const lastIdEl=document.getElementById("lastId");
const bitrateLabel=document.getElementById("bitrateLabel");
const modeLabel=document.getElementById("modeLabel");
const errorLabel=document.getElementById("errorLabel");
const idEl=document.getElementById("canId");
const dlcEl=document.getElementById("dlc");
const dataEl=document.getElementById("data");
const extEl=document.getElementById("ext");
const rtrEl=document.getElementById("rtr");
const bitrateEl=document.getElementById("bitrate");
const modeEl=document.getElementById("mode");
const showRxEl=document.getElementById("showRx");
const showTxEl=document.getElementById("showTx");
const filterIdEl=document.getElementById("filterId");
const clearFramesBtn=document.getElementById("clearFrames");
const interfaceNav=document.getElementById("interfaceNav");
const pageTitle=document.getElementById("pageTitle");
let ws=null;
let statusTimer=null;
let connectWatch=null;
let rxCount=0,txCount=0,errCount=0;
let frames=[];
let configDirty=false;
function now(){return new Date().toLocaleTimeString()}
function log(line){logEl.textContent+=now()+"  "+line+"\n";logEl.scrollTop=logEl.scrollHeight}
function setStatus(kind,text){statusEl.className="status "+kind;statusText.textContent=text}
function setConnected(on){connectBtn.disabled=on;disconnectBtn.disabled=!on;sendBtn.disabled=!on;applyConfigBtn.disabled=!on;refreshStatusBtn.disabled=!on}
function clearConnectWatch(){if(connectWatch){clearTimeout(connectWatch);connectWatch=null}}
function parseId(text){const s=text.trim();return Number.parseInt(s,/^0x/i.test(s)?16:10)}
function parseData(text){const s=text.trim();if(!s)return[];return s.split(/[ ,]+/).filter(Boolean).map(v=>Number.parseInt(v,/^0x/i.test(v)?16:16))}
function fmtId(id,ext){return "0x"+Number(id).toString(16).toUpperCase().padStart(ext?8:3,"0")}
function fmtData(data){return (data||[]).map(v=>Number(v).toString(16).toUpperCase().padStart(2,"0")).join(" ")}
function esc(s){return String(s).replace(/[&<>]/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;"}[c]))}
function val(v){return v==null?"-":v}
function updateCounts(){rxCountEl.textContent=rxCount;txCountEl.textContent=txCount}
function stateView(state){
 if(state==="active"||state==="error-active")return["Active","stateOk"];
 if(state==="error-warning")return["Warning","stateWarn"];
 if(state==="error-passive")return["Passive","stateWarn"];
 if(state==="bus-off"||state==="error")return[state==="bus-off"?"Bus off":"Error","stateBad"];
 if(state==="stopped")return["Stopped","stateOff"];
 return[state||"-",""];
}
function updateStatus(msg){const st=stateView(msg.state);canStateEl.textContent=st[0];canStateEl.className=st[1];txErrCountEl.textContent=msg.txErr||0;rxErrCountEl.textContent=msg.rxErr||0;txQueueEl.textContent=val(msg.txQueueUsed)+"/"+val(msg.txQueueFree);rxQueueEl.textContent=val(msg.rxQueueUsed)+"/"+val(msg.rxQueueFree);if(!configDirty){if(msg.bitrate)bitrateEl.value=msg.bitrate;if(msg.mode)modeEl.value=msg.mode}bitrateLabel.textContent=msg.bitrate?"CAN "+Math.round(msg.bitrate/1000)+" kbit/s":"CAN -";modeLabel.textContent="mode "+(msg.mode||"-");errorLabel.textContent="TX err "+(msg.txErr||0)+" / RX err "+(msg.rxErr||0)+" | TXQ "+val(msg.txQueueUsed)+"/"+val(msg.txQueueFree)+" | RXQ "+val(msg.rxQueueUsed)+"/"+val(msg.rxQueueFree)}
function requestStatus(){if(ws&&ws.readyState===1)ws.send(JSON.stringify({type:"can.status"}))}
function filterIdValue(){const s=filterIdEl.value.trim();return s?parseId(s):null}
function frameVisible(item){if(item.dir==="RX"&&!showRxEl.checked)return false;if(item.dir==="TX"&&!showTxEl.checked)return false;const wanted=filterIdValue();return wanted==null||Number(item.msg.id)===wanted}
function rowHtml(item){const msg=item.msg;const cls=item.dir==="RX"?"dirRx":item.dir==="TX"?"dirTx":"dirErr";const dlc=msg.dlc==null?"":msg.dlc;return "<td>"+item.time+"</td><td class=\""+cls+"\">"+item.dir+"</td><td>"+esc(fmtId(msg.id||0,msg.ext))+"</td><td>"+(msg.ext?"EXT":"STD")+(msg.rtr?" RTR":"")+"</td><td>"+esc(dlc)+"</td><td>"+esc(fmtData(msg.data))+"</td>"}
function renderFrames(){framesEl.textContent="";for(const item of frames){if(!frameVisible(item))continue;const tr=document.createElement("tr");tr.innerHTML=rowHtml(item);framesEl.appendChild(tr)}}
function addFrame(dir,msg){frames.unshift({dir,msg,time:now()});while(frames.length>160)frames.pop();lastIdEl.textContent=fmtId(msg.id||0,msg.ext);renderFrames()}
function makeFrame(){const data=parseData(dataEl.value);const dlc=Number.parseInt(dlcEl.value,10);if(data.some(v=>!Number.isInteger(v)||v<0||v>255))throw new Error("Invalid data byte");if(!Number.isInteger(dlc)||dlc<0||dlc>8)throw new Error("Invalid DLC");if(!rtrEl.checked&&data.length!==dlc)throw new Error("Data length must match DLC");const id=parseId(idEl.value);if(!Number.isInteger(id)||id<0)throw new Error("Invalid ID");return{type:"can.tx",bus:0,id,ext:extEl.checked,rtr:rtrEl.checked,dlc,data:rtrEl.checked?[]:data}}
async function copyText(text){if(navigator.clipboard&&navigator.clipboard.writeText){await navigator.clipboard.writeText(text);return}const ta=document.createElement("textarea");ta.value=text;ta.style.position="fixed";ta.style.opacity="0";document.body.appendChild(ta);ta.focus();ta.select();document.execCommand("copy");ta.remove()}
function connect(){
 if(ws&&ws.readyState<2)return;
 setStatus("wait","connecting");
 connectBtn.disabled=true;
 disconnectBtn.disabled=false;
 const sock=new WebSocket("ws://"+location.host+"/can");
 ws=sock;
 clearConnectWatch();
 connectWatch=setTimeout(()=>{if(ws===sock&&sock.readyState===0){log("connect timeout");ws=null;setStatus("bad","disconnected");setConnected(false);sock.close()}},5000);
 sock.onopen=()=>{if(ws!==sock)return;clearConnectWatch();setStatus("ok","connected");setConnected(true);log("connected");requestStatus();statusTimer=setInterval(requestStatus,2000)};
 sock.onmessage=e=>{if(ws!==sock)return;try{const msg=JSON.parse(e.data);if(msg.type!=="can.status")log("< "+e.data);if(msg.type==="can.rx"){rxCount++;addFrame("RX",msg)}else if(msg.type==="can.tx"){txCount++;addFrame("TX",msg)}else if(msg.type==="can.status"){updateStatus(msg)}else if(msg.type==="error"){errCount++;addFrame("ERR",msg)}updateCounts()}catch(_){log("< "+e.data)}};
 sock.onclose=e=>{if(ws===sock)ws=null;clearConnectWatch();if(statusTimer){clearInterval(statusTimer);statusTimer=null}setStatus("bad","disconnected");setConnected(false);log("closed "+e.code)};
 sock.onerror=()=>{if(ws!==sock)return;clearConnectWatch();setStatus("bad","error");errCount++;updateCounts();log("error")};
}
connectBtn.onclick=connect;
disconnectBtn.onclick=()=>{if(ws)ws.close()};
sendBtn.onclick=()=>{try{const frame=makeFrame();const text=JSON.stringify(frame);if(ws&&ws.readyState===1){ws.send(text);log("> "+text)}}catch(e){errCount++;updateCounts();log("! "+e.message)}};
copyJsonBtn.onclick=async()=>{try{const text=JSON.stringify(makeFrame());await copyText(text);log("copied "+text)}catch(e){errCount++;updateCounts();log("! "+e.message)}};
bitrateEl.oninput=()=>{configDirty=true};
modeEl.onchange=()=>{configDirty=true};
applyConfigBtn.onclick=()=>{const bitrate=Number.parseInt(bitrateEl.value,10);if(!Number.isInteger(bitrate)||bitrate<=0){log("! Invalid bitrate");return}const msg={type:"can.config.set",bitrate,mode:modeEl.value};const text=JSON.stringify(msg);if(ws&&ws.readyState===1){configDirty=false;ws.send(text);log("> "+text)}};
refreshStatusBtn.onclick=()=>{configDirty=false;requestStatus()};
showRxEl.onchange=renderFrames;
showTxEl.onchange=renderFrames;
filterIdEl.oninput=renderFrames;
clearFramesBtn.onclick=()=>{frames=[];framesEl.textContent="";lastIdEl.textContent="-"};
clearBtn.onclick=()=>{logEl.textContent="";frames=[];framesEl.textContent="";rxCount=0;txCount=0;errCount=0;lastIdEl.textContent="-";updateCounts()};
fetch("/api/status").then(response=>response.json()).then(status=>{if(status.board){const title="Pico I/O Bridge - "+status.board;pageTitle.textContent=title;document.title=title}for(const [name,path] of Object.entries(status.pages||{})){if(name==="can")continue;const link=document.createElement("a");link.className="navlink";link.href=path;link.textContent=name.toUpperCase();interfaceNav.appendChild(link)}}).catch(()=>{});
log("ready");
setTimeout(connect,100);
</script>
</body>
</html>
"#;
            write_http_response(socket, "text/html", BODY).await?;
        }
        #[cfg(all(not(feature = "can"), feature = "i2c"))]
        {
            const BODY: &[u8] = include_bytes!("i2c.html");
            write_http_response(socket, "text/html", BODY).await?;
        }
    } else {
        write_not_found(socket).await?;
    }

    Ok(())
}

#[cfg(feature = "can")]
async fn can_websocket_loop(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8],
) -> Result<(), embassy_net::tcp::Error> {
    const READY: &[u8] = b"{\"type\":\"hello\",\"ok\":true,\"endpoint\":\"/can\"}";
    let mut response = String::<256>::new();
    let Ok(mut can_events) = CAN_EVENTS.subscriber() else {
        return Ok(());
    };

    websocket::send_text(socket, READY).await?;

    loop {
        if !socket.may_recv() {
            return Ok(());
        }

        let ready_or_event = select(socket.wait_read_ready(), can_events.next_message_pure());
        let incoming = match select(ready_or_event, Timer::after(Duration::from_millis(100))).await
        {
            Either::First(Either::First(())) => websocket::read_frame(socket, buf).await?,
            Either::First(Either::Second(CanEvent::Rx(frame))) => {
                response.clear();
                write_can_frame_json(&mut response, "can.rx", true, frame);
                websocket::send_text(socket, response.as_bytes()).await?;
                continue;
            }
            Either::Second(()) => continue,
        };

        let Some(frame) = incoming else {
            return Ok(());
        };

        match frame {
            Frame::Close => {
                websocket::send_close(socket).await?;
                return Ok(());
            }
            Frame::Ping(payload) => websocket::send_pong(socket, payload).await?,
            Frame::Pong => {}
            Frame::Text(payload) => {
                response.clear();
                handle_can_ws_text(payload, &mut response).await;
                websocket::send_text(socket, response.as_bytes()).await?;
            }
            Frame::Binary => {
                const RESPONSE: &[u8] = b"{\"type\":\"error\",\"ok\":false,\"code\":\"unsupported_type\",\"message\":\"binary CAN messages are not supported yet\"}";
                websocket::send_text(socket, RESPONSE).await?;
            }
        }
    }
}

#[cfg(feature = "i2c")]
async fn i2c_websocket_loop(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8],
) -> Result<(), embassy_net::tcp::Error> {
    const READY: &[u8] = b"{\"type\":\"hello\",\"ok\":true,\"endpoint\":\"/i2c\"}";
    let mut response = String::<512>::new();

    websocket::send_text(socket, READY).await?;

    loop {
        let Some(frame) = websocket::read_frame(socket, buf).await? else {
            return Ok(());
        };

        match frame {
            Frame::Close => {
                websocket::send_close(socket).await?;
                return Ok(());
            }
            Frame::Ping(payload) => websocket::send_pong(socket, payload).await?,
            Frame::Pong => {}
            Frame::Text(payload) => {
                response.clear();
                handle_i2c_ws_text(payload, &mut response).await;
                websocket::send_text(socket, response.as_bytes()).await?;
            }
            Frame::Binary => {
                const RESPONSE: &[u8] = b"{\"type\":\"error\",\"ok\":false,\"code\":\"unsupported_type\",\"message\":\"binary I2C messages are not supported yet\"}";
                websocket::send_text(socket, RESPONSE).await?;
            }
        }
    }
}

#[embassy_executor::task(pool_size = HTTP_SOCKETS)]
pub(crate) async fn http_task(stack: embassy_net::Stack<'static>, serial: &'static str) {
    let mut rx_buf = [0; 2048];
    let mut tx_buf = [0; 2048];
    let mut request_buf = [0; 1024];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);

    loop {
        socket.set_timeout(Some(Duration::from_secs(10)));
        socket.set_keep_alive(None);
        socket.set_nagle_enabled(false);

        if socket.accept(crate::HTTP_PORT).await.is_ok() {
            defmt::info!("HTTP client connected");
            match serve_http_connection(&mut socket, &mut request_buf, serial).await {
                Ok(()) => {
                    close_gracefully(&mut socket).await;
                }
                Err(_) => {
                    socket.abort();
                    let _ = socket.flush().await;
                }
            }
        } else {
            socket.abort();
            let _ = socket.flush().await;
        }

        Timer::after(Duration::from_millis(50)).await;
    }
}
