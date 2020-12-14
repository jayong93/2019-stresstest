use anyhow::{anyhow, bail, ensure, Context, Error, Result};
use crossterm::{
    event::{self, Event as CEvent, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use evmap::{self, Options, ReadHandle, WriteHandle};
use num::FromPrimitive;
use rand::prelude::*;
use std::collections::hash_map::RandomState;
use std::io::{stdout, Write};
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::string::ToString;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicUsize, Ordering};
use std::sync::{
    mpsc::{channel, Receiver, Sender},
    Arc,
};
use std::{
    cmp::max,
    fs::{File, OpenOptions},
    path::PathBuf,
    time::{Duration, Instant, UNIX_EPOCH},
};
use structopt::StructOpt;
use tokio::{io::BufReader, net, prelude::*, time};
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::Color,
    widgets::{
        canvas::{Canvas, Points},
        Block, Borders, List, Paragraph, Text,
    },
    Terminal,
};
use lazy_static::lazy_static;

mod packet;

type PlayerMapRead = ReadHandle<i32, Arc<Player>, (), RandomState>;

static mut MAX_TEST: u64 = 0;
static mut BOARD_WIDTH: usize = 0;
static mut BOARD_HEIGHT: usize = 0;
static mut PORT: u16 = 0;
static mut CALC_EXTERNAL_DELAY: bool = false;
static mut TELEPORT_ON_LOGIN: bool = false;
static mut LOG_ALL_DELAY: bool = false;
static mut SERVER_THREAD_NUM: Option<NonZeroUsize> = None;
static PLAYER_NUM: AtomicUsize = AtomicUsize::new(0);
static INTERNAL_DELAY: AtomicIsize = AtomicIsize::new(0);
static EXTERNAL_DELAY: AtomicIsize = AtomicIsize::new(0);
static TOTAL_INTERNAL_DELAY_IN_1S: AtomicIsize = AtomicIsize::new(0);
static TOTAL_INTERNAL_DELAY_COUNT_IN_1S: AtomicIsize = AtomicIsize::new(0);
static TOTAL_EXTERNAL_DELAY_IN_1S: AtomicIsize = AtomicIsize::new(0);
static TOTAL_EXTERNAL_DELAY_COUNT_IN_1S: AtomicIsize = AtomicIsize::new(0);
static TOTAL_PACKET_COUNT_IN_1S: AtomicIsize = AtomicIsize::new(0);
static mut TOTAL_INTERNAL_DELAY_PER_THREAD: Vec<AtomicIsize> = Vec::new();
static mut TOTAL_INTERNAL_DELAY_COUNT_PER_THREAD: Vec<AtomicIsize> = Vec::new();

lazy_static! {
    static ref RUNTIME: tokio::runtime::Runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
}

#[derive(Debug)]
struct Player {
    id: i32,
    position: AtomicU32,
    is_alive: AtomicBool,
}

// recv_buf, packet_buf, last_move_time은 receiver에서만 접근.
// receiver는 한번에 한 thread만 호출할 수 있으므로 안전.
unsafe impl Sync for Player {}

impl PartialEq for Player {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Player {}

impl std::hash::Hash for Player {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

const RECV_SIZE: usize = 1024;

impl Player {
    fn new(id: i32, x: i16, y: i16) -> Self {
        Player {
            id,
            position: AtomicU32::new(Self::compose_position(x, y)),
            is_alive: AtomicBool::new(true),
        }
    }
    fn compose_position(x: i16, y: i16) -> u32 {
        ((x as u32) << 16) | y as u32
    }
    fn get_pos(&self) -> (i16, i16) {
        let pos = self.position.load(Ordering::Relaxed);
        let mask = 0xffff;
        let x = (pos >> 16) as i16;
        let y = (pos & mask) as i16;
        (x, y)
    }
    fn set_pos(&self, x: i16, y: i16) {
        let new_pos = Self::compose_position(x, y);
        self.position.store(new_pos, Ordering::SeqCst);
    }
}

fn from_bytes<T>(bytes: &[u8]) -> &T {
    if bytes.len() != std::mem::size_of::<T>() {
        panic!(
            "It's not a compatible type, the bytes have {} bytes, the type is {} bytes",
            bytes.len(),
            std::mem::size_of::<T>()
        );
    }
    unsafe { &*(bytes.as_ptr() as *const T) }
}

async fn process_login(
    stream: &mut net::TcpStream,
    write_handle: &mut WriteHandle<i32, Arc<Player>, (), RandomState>,
) -> Result<Arc<Player>> {
    let mut read_buf = vec![0; 256];
    stream
        .read_exact(&mut read_buf[0..1])
        .await
        .context("Can't read packet size from server")?;
    let total_size = read_buf[0] as usize;
    stream
        .read_exact(&mut read_buf[1..total_size])
        .await
        .context("Can't read packets from server")?;

    use packet::*;
    match FromPrimitive::from_u8(read_buf[1]) {
        Some(SCPacketType::LoginOk) => {
            let p = from_bytes::<SCLoginOk>(&read_buf[0..total_size]);
            let id = p.id;
            let player = Arc::new(Player::new(id, p.x, p.y));
            write_handle.insert(id, player.clone());
            write_handle.refresh();
            Ok(player)
        }
        Some(p) => bail!(
            "the server has sent a packet that isn't a login packet. It was {:#?}",
            p
        ),
        _ => bail!("the server has sent unknown packet type {}", read_buf[1]),
    }
}

fn assemble_packet(
    player: &Player,
    buf: &mut [u8],
    prev_packet_size: &mut usize,
    received_size: usize,
    delay_send: &Sender<isize>,
) -> Result<()> {
    let recv_buf = &mut buf[..(*prev_packet_size + received_size)];
    let mut packet_head_idx = 0;

    while packet_head_idx < recv_buf.len() {
        let packet_size = recv_buf[packet_head_idx] as usize;

        if packet_size < 2 {
            bail!("a packet size was less than 2. It was {}", packet_size);
        }

        let packet_tail_idx = packet_head_idx + packet_size;
        if packet_tail_idx <= recv_buf.len() {
            process_packet(
                &recv_buf[packet_head_idx..packet_tail_idx],
                player,
                delay_send,
            )?;
            TOTAL_PACKET_COUNT_IN_1S.fetch_add(1, Ordering::Relaxed);
            packet_head_idx = packet_tail_idx;
            *prev_packet_size = 0;
        } else {
            recv_buf.copy_within(packet_head_idx.., 0);
            *prev_packet_size = recv_buf.len() - packet_head_idx;
            break;
        }
    }
    Ok(())
}

fn adjust_delay(
    id: isize,
    counter: &AtomicIsize,
    accumulator: &AtomicIsize,
    delay_acc_counter: &AtomicIsize,
    move_time: u32,
    delay_send: &Sender<isize>,
) -> Result<()> {
    use std::time::SystemTime;
    let curr_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u32;
    if curr_time < move_time {
        return Ok(());
    }

    let d_ms = curr_time as isize - move_time as isize;
    let delay = counter.load(Ordering::Relaxed);
    let mut cur_delay = delay;
    if delay < d_ms {
        cur_delay = counter.fetch_add(d_ms - delay, Ordering::Relaxed) + (d_ms - delay);
    } else if delay > d_ms {
        cur_delay = counter.fetch_sub(delay - d_ms, Ordering::Relaxed) - (delay - d_ms);
    }

    if unsafe { LOG_ALL_DELAY } {
        delay_send.send(d_ms)?;
    }

    if let Some(thread_num) =
        unsafe { SERVER_THREAD_NUM.filter(|_| id >= 0).map(NonZeroUsize::get) }
    {
        let tid = id as usize % thread_num;
        unsafe {
            TOTAL_INTERNAL_DELAY_PER_THREAD[tid].fetch_add(cur_delay, Ordering::Relaxed);
            TOTAL_INTERNAL_DELAY_COUNT_PER_THREAD[tid].fetch_add(1, Ordering::Relaxed);
        }
    } else {
        accumulator.fetch_add(cur_delay, Ordering::Relaxed);
        delay_acc_counter.fetch_add(1, Ordering::Relaxed);
    }

    Ok(())
}

fn process_packet(packet: &[u8], player: &Player, delay_send: &Sender<isize>) -> Result<()> {
    use packet::{SCPacketType, SCPosPlayer};
    let packet_type = *packet.get(1).ok_or(anyhow!("packet has no type field"))?;
    match FromPrimitive::from_u8(packet_type) {
        Some(SCPacketType::Pos) => {
            let p = from_bytes::<SCPosPlayer>(packet);
            let id = p.id;

            if player.id == id {
                player.set_pos(p.x as i16, p.y as i16);
                if p.move_time != 0 {
                    adjust_delay(
                        player.id as isize,
                        &INTERNAL_DELAY,
                        &TOTAL_INTERNAL_DELAY_IN_1S,
                        &TOTAL_INTERNAL_DELAY_COUNT_IN_1S,
                        p.move_time,
                        delay_send,
                    )?;
                }
            } else if unsafe { CALC_EXTERNAL_DELAY } && p.move_time != 0 {
                adjust_delay(
                    -1,
                    &EXTERNAL_DELAY,
                    &TOTAL_EXTERNAL_DELAY_IN_1S,
                    &TOTAL_EXTERNAL_DELAY_COUNT_IN_1S,
                    p.move_time,
                    delay_send,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn read_packet(
    stream: &mut BufReader<net::tcp::OwnedReadHalf>,
    buf: &mut [u8],
    prev_packet_size: &mut usize,
) -> Result<usize> {
    let read_size = stream.read(&mut buf[*prev_packet_size..]).await?;
    ensure!(read_size > 0, "Read zero byte");
    Ok(read_size)
}

async fn receiver(
    stream: net::tcp::OwnedReadHalf,
    player: Arc<Player>,
    err_send: Sender<Error>,
    delay_send: Sender<isize>,
) {
    let mut stream = BufReader::new(stream);
    let mut buf = Vec::with_capacity(RECV_SIZE);
    unsafe {
        buf.set_len(RECV_SIZE);
    }
    let mut prev_packet_size = 0usize;
    while player.is_alive.load(Ordering::Relaxed) {
        match read_packet(&mut stream, &mut buf, &mut prev_packet_size)
            .await
            .and_then(|read_size| {
                assemble_packet(
                    player.as_ref(),
                    &mut buf,
                    &mut prev_packet_size,
                    read_size,
                    &delay_send,
                )
            }) {
            Ok(_) => {}
            Err(e) => {
                err_send.send(e).expect("Can't send error message");
            }
        }
    }
}

async fn sender(
    mut stream: net::tcp::OwnedWriteHalf,
    player: Arc<Player>,
    err_send: Sender<Error>,
    sleep_time: Duration,
) {
    if unsafe { TELEPORT_ON_LOGIN } {
        let tele_packet = packet::CSTeleport::new();
        let p_bytes = unsafe {
            std::slice::from_raw_parts(
                &tele_packet as *const packet::CSTeleport as *const u8,
                std::mem::size_of::<packet::CSTeleport>(),
            )
        };
        if let Err(e) = stream
            .write_all(p_bytes)
            .await
            .context("Can't send teleport packet")
        {
            err_send.send(e).expect("Can't send error message");
            return;
        }
    }

    let mut packets = [
        packet::CSMove::new(packet::Direction::Up, 0),
        packet::CSMove::new(packet::Direction::Down, 0),
        packet::CSMove::new(packet::Direction::Left, 0),
        packet::CSMove::new(packet::Direction::Right, 0),
    ];
    let mut rng = StdRng::from_entropy();
    while player.is_alive.load(Ordering::Relaxed) {
        let picked_packet = &mut packets[rng.gen_range(0, packets.len())];
        picked_packet.move_time = UNIX_EPOCH.elapsed().unwrap().as_millis() as u32;

        let bytes = unsafe {
            std::slice::from_raw_parts(
                picked_packet as *const packet::CSMove as *const u8,
                std::mem::size_of::<packet::CSMove>(),
            )
        };
        if let Err(e) = stream
            .write_all(bytes)
            .await
            .context("Can't send to server")
        {
            err_send.send(e).expect("Can't send error message");
            return;
        }
        time::sleep(sleep_time).await;
    }
}

fn disconnect_client(client_id: i32, read_handle: &PlayerMapRead) {
    if let Some(values) = read_handle.get(&client_id) {
        let player = values
            .iter()
            .next()
            .expect("Can't disconnect client, because can't find player");
        player.is_alive.store(false, Ordering::Relaxed);
        PLAYER_NUM.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug, StructOpt)]
struct CmdOption {
    #[structopt(long, default_value = "400")]
    board_width: u16,

    #[structopt(long, default_value = "400")]
    board_height: u16,

    #[structopt(short, long, default_value = "10000")]
    max_player: usize,

    #[structopt(short, long, default_value = "9000")]
    port: u16,

    #[structopt(long)]
    use_external_delay: bool,

    #[structopt(long)]
    teleport_on_login: bool,

    #[structopt(long, default_value = "50")]
    accept_delay: usize,

    #[structopt(long, default_value = "1000")]
    move_cycle: u64,

    #[structopt(long, default_value = "2")]
    accept_delay_multiplier: usize,

    #[structopt(long, default_value = "100")]
    delay_threshold: usize,

    #[structopt(short, long, default_value = "127.0.0.1")]
    ip_addr: String,

    #[structopt(long, default_value = "delay.log")]
    delay_log: PathBuf,

    #[structopt(long)]
    server_thread_num: Option<NonZeroUsize>,

    #[structopt(long, conflicts_with = "server-thread-num")]
    log_all_delay: bool,
}

enum Event<I> {
    Key(I),
    Tick,
    Err(Error),
}

use std::ffi::{OsStr, OsString};
fn main() -> Result<()> {
    let opt = CmdOption::from_args();
    unsafe {
        MAX_TEST = opt.max_player as u64;
        BOARD_WIDTH = opt.board_width as usize;
        BOARD_HEIGHT = opt.board_height as usize;
        PORT = opt.port;
        CALC_EXTERNAL_DELAY = opt.use_external_delay;
        TELEPORT_ON_LOGIN = opt.teleport_on_login;
        SERVER_THREAD_NUM = opt.server_thread_num;
        LOG_ALL_DELAY = opt.log_all_delay;

        if let Some(thread_num) = opt.server_thread_num {
            TOTAL_INTERNAL_DELAY_PER_THREAD.resize_with(thread_num.get(), Default::default);
            TOTAL_INTERNAL_DELAY_COUNT_PER_THREAD.resize_with(thread_num.get(), Default::default);
        }
    }

    std::sync::atomic::fence(Ordering::SeqCst);

    let mut file_vec = Vec::new();
    let mut open_option = OpenOptions::new();
    open_option.write(true).truncate(true).create(true);
    if let Some(thread_num) = opt.server_thread_num.map(NonZeroUsize::get) {
        let mut file_path: PathBuf = opt.delay_log.clone();
        let file_name_base: String = opt
            .delay_log
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap()
            .to_owned();
        for tid in 0..thread_num {
            file_path.set_file_name::<OsString>(
                (file_name_base.clone() + &format!("_{}T.log", tid + 1)).into(),
            );
            let file = open_option.open(&file_path).unwrap();
            file_vec.push(file);
            writeln!(
                &mut file_vec[tid],
                "INTERNAL_DELAY, EXTERNAL_DELAY, PACKETS"
            )
            .unwrap();
        }
    } else {
        let log_file = open_option.open(&opt.delay_log).unwrap();
        file_vec.push(log_file);
        writeln!(&mut file_vec[0], "INTERNAL_DELAY, EXTERNAL_DELAY, PACKETS").unwrap();
    }

    let (read_handle, mut write_handle) = Options::default()
        .with_capacity(unsafe { MAX_TEST } as usize)
        .construct();

    let (err_send, err_recv) = channel();
    let (delay_send, mut delay_recv) = channel();

    let server = move || {
        let curr_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut handle = None;
        let mut ip_addr =
            std::net::SocketAddrV4::new(FromStr::from_str(opt.ip_addr.as_str()).unwrap(), unsafe {
                PORT
            });
        ip_addr.set_port(unsafe { PORT });

        let mut last_login_time = Instant::now();
        let mut delay_multiplier = 1;
        let mut is_increasing = true;
        let accept_delay = opt.accept_delay as u128;
        let move_cycle = Duration::from_millis(opt.move_cycle);
        let mut client_to_disconnect = 0;	
        let mut max_player_num = unsafe { MAX_TEST };
        let delay_threshold = opt.delay_threshold as usize;	
        let delay_threshold2 = (delay_threshold as f64 * 1.5) as usize;
        while PLAYER_NUM.load(Ordering::Relaxed) < unsafe { MAX_TEST } as usize {
            // 접속 가능 여부 판단
            let elapsed_time = last_login_time.elapsed().as_millis();
            if elapsed_time < (accept_delay * delay_multiplier) {
                continue;
            }

            if delay_threshold > 0 {	
                let internal_delay = INTERNAL_DELAY.load(Ordering::Relaxed);	
                let cur_player_num = PLAYER_NUM.load(Ordering::Relaxed) as u64;	
                if delay_threshold2 < internal_delay as _ {	
                    if is_increasing {	
                        max_player_num = cur_player_num;	
                        is_increasing = false;	
                    }	
                    if cur_player_num < 100 {	
                        continue;	
                    }	
                    if elapsed_time < accept_delay * 2 {	
                        continue;	
                    }	

                    last_login_time = Instant::now();	
                    disconnect_client(client_to_disconnect, &mut write_handle);	
                    client_to_disconnect += 1;	
                    continue;	
                } else if delay_threshold < internal_delay as _ {	
                    delay_multiplier = opt.accept_delay_multiplier as u128;	
                    continue;	
                }	

                if max_player_num != unsafe { MAX_TEST }	
                    && max_player_num - (max_player_num / 20) < cur_player_num	
                {	
                    continue;	
                }	

                is_increasing = true;	
            }
            last_login_time = Instant::now();

            match curr_runtime.block_on(net::TcpStream::connect(ip_addr)) {
                Ok(mut client) => {
                    client.set_nodelay(true).ok();

                    let login_packet = packet::CSLogin::new();
                    let p_bytes = unsafe {
                        std::slice::from_raw_parts(
                            &login_packet as *const packet::CSLogin as *const u8,
                            std::mem::size_of::<packet::CSLogin>(),
                        )
                    };
                    if let None = curr_runtime
                        .block_on(client.write_all(p_bytes))
                        .context("Can't send login packet")
                        .map_err(|e| err_send.send(e))
                        .ok()
                    {
                        continue;
                    }

                    if let Ok(player) = curr_runtime
                        .block_on(process_login(&mut client, &mut write_handle))
                        .map_err(|e| err_send.send(e))
                    {
                        let delay_send = delay_send.clone();
                        let err_send = err_send.clone();
                        let (read_half, write_half) = client.into_split();
                        handle = Some(RUNTIME.spawn(async move {
                            let recv = RUNTIME.spawn(receiver(
                                read_half,
                                player.clone(),
                                err_send.clone(),
                                delay_send,
                            ));
                            let send =
                                RUNTIME.spawn(sender(write_half, player, err_send, move_cycle));
                            recv.await.ok();
                            send.await.ok();
                        }));
                        PLAYER_NUM.fetch_add(1, Ordering::Relaxed);
                    } else {
                        continue;
                    }
                }
                Err(e) => {
                    err_send
                        .send(anyhow!("Can't connect to server: {}", e))
                        .expect("Can't send error message");
                }
            }
        }

        curr_runtime.block_on(handle.unwrap()).ok();
    };
    RUNTIME.spawn_blocking(server);

    let (tx, rx) = channel();

    if unsafe { LOG_ALL_DELAY } {
        RUNTIME.spawn_blocking(move || {
            blocking_main(tx, err_recv, |elapsed_time_up_to_1s: &Duration| {
                log_all_delay(&mut file_vec[0], elapsed_time_up_to_1s, &mut delay_recv)
            });
        });
    } else if let Some(thread_num) = unsafe { SERVER_THREAD_NUM } {
        RUNTIME.spawn_blocking(move || {
            blocking_main(tx, err_recv, |elapsed_time: &Duration| {
                for tid in 0..thread_num.get() {
                    log_delay_per_thread(tid, &mut file_vec[tid], elapsed_time);
                }
            })
        });
    } else {
        RUNTIME.spawn_blocking(move || {
            blocking_main(tx, err_recv, |elapsed_time: &Duration| {
                log_delay(&mut file_vec[0], elapsed_time)
            })
        });
    }

    let ui_main_fut;
    {
        let read_handle = read_handle.clone();
        ui_main_fut = RUNTIME.spawn_blocking(move || {
            enable_raw_mode().expect("Can't use raw mode");
            let mut stdout = stdout();
            execute!(stdout, EnterAlternateScreen).expect("Can't enter to alternate screen");
            let backend = CrosstermBackend::new(stdout);
            let mut terminal = Terminal::new(backend).expect("Can't create a terminal");
            terminal.hide_cursor().expect("Can't hide a cursor");
            let mut err_vec = Vec::new();

            while let Ok(e) = rx.recv() {
                match e {
                    Event::Tick => {
                        let pos_vec: Vec<_> = read_handle
                            .read()
                            .iter()
                            .filter_map(|(_, players)| {
                                if let Some(player) = players.iter().next() {
                                    if !player.is_alive.load(Ordering::Relaxed) {
                                        return None;
                                    }
                                    let (x, y) = player.get_pos();
                                    Some((x as f64, y as f64))
                                } else {
                                    None
                                }
                            })
                            .collect();
                        terminal
                            .draw(|mut f| {
                                let layout = Layout::default()
                                    .direction(Direction::Vertical)
                                    .constraints(
                                        [
                                            Constraint::Length(5),
                                            Constraint::Min(0),
                                            Constraint::Length(6),
                                        ]
                                        .as_ref(),
                                    )
                                    .split(f.size());
                                let text = [Text::Raw(
                                    if unsafe { CALC_EXTERNAL_DELAY } {
                                        format!(
                                        "Internal Delay: {} ms\nExternal Dalay: {} ms\nPlayers: {}",
                                        INTERNAL_DELAY.load(Ordering::Relaxed),
                                        EXTERNAL_DELAY.load(Ordering::Relaxed),
                                        PLAYER_NUM.load(Ordering::Relaxed),
                                    )
                                    } else {
                                        format!(
                                            "Delay: {} ms\nPlayers: {}",
                                            INTERNAL_DELAY.load(Ordering::Relaxed),
                                            PLAYER_NUM.load(Ordering::Relaxed),
                                        )
                                    }
                                    .into(),
                                )];
                                let para = Paragraph::new(text.iter())
                                    .block(Block::default().borders(Borders::ALL).title("Info"))
                                    .wrap(true);
                                f.render_widget(para, layout[0]);
                                let canvas = Canvas::default()
                                    .block(Block::default().borders(Borders::ALL).title("World"))
                                    .paint(|ctx| {
                                        let points = Points {
                                            coords: pos_vec.as_slice(),
                                            color: Color::White,
                                        };
                                        ctx.draw(&points);
                                    })
                                    .x_bounds([0.0, unsafe { BOARD_WIDTH as f64 }])
                                    .y_bounds([0.0, unsafe { BOARD_HEIGHT as f64 }]);
                                f.render_widget(canvas, layout[1]);
                                let list =
                                    List::new(err_vec.iter().enumerate().rev().take(10).map(
                                        |(i, s): (_, &String)| {
                                            Text::Raw(std::borrow::Cow::Owned(format!(
                                                "#{}: {}",
                                                i, s
                                            )))
                                        },
                                    ))
                                    .block(Block::default().borders(Borders::ALL).title("Errors"));
                                f.render_widget(list, layout[2]);
                            })
                            .expect("Can't draw to screen");
                    }
                    Event::Key(key_event)
                        if key_event.code == KeyCode::Char('q')
                            || (key_event.modifiers == KeyModifiers::CONTROL
                                && key_event.code == KeyCode::Char('c')) =>
                    {
                        disable_raw_mode().expect("Can't disable raw mode");
                        execute!(terminal.backend_mut(), LeaveAlternateScreen)
                            .expect("Can't leave alternate screen");
                        terminal.show_cursor().expect("Can't show cursor");
                        break;
                    }
                    Event::Err(e) => err_vec.push(e.to_string()),
                    _ => {}
                }
            }
        });
    }
    let result = RUNTIME.block_on(ui_main_fut);

    for (id, _) in read_handle.read().iter() {
        disconnect_client(*id, &read_handle);
    }

    result.map_err(|e| anyhow!(e))
}

fn log_delay(file: &mut File, elapsed_time: &Duration) {
    if PLAYER_NUM.load(Ordering::Relaxed) >= unsafe { MAX_TEST } as usize {
        if elapsed_time.as_secs() >= 1 {
            writeln!(
                file,
                "{}, {}, {}",
                TOTAL_INTERNAL_DELAY_IN_1S.load(Ordering::Relaxed) as f32
                    / max(1, TOTAL_INTERNAL_DELAY_COUNT_IN_1S.load(Ordering::Relaxed)) as f32,
                TOTAL_EXTERNAL_DELAY_IN_1S.load(Ordering::Relaxed) as f32
                    / max(1, TOTAL_EXTERNAL_DELAY_COUNT_IN_1S.load(Ordering::Relaxed)) as f32,
                TOTAL_PACKET_COUNT_IN_1S.load(Ordering::Relaxed)
            )
            .unwrap();
            TOTAL_EXTERNAL_DELAY_IN_1S.store(0, Ordering::Relaxed);
            TOTAL_INTERNAL_DELAY_IN_1S.store(0, Ordering::Relaxed);
            TOTAL_EXTERNAL_DELAY_COUNT_IN_1S.store(0, Ordering::Relaxed);
            TOTAL_INTERNAL_DELAY_COUNT_IN_1S.store(0, Ordering::Relaxed);
            TOTAL_PACKET_COUNT_IN_1S.store(0, Ordering::Relaxed);
        }
    }
}

fn log_delay_per_thread(tid: usize, file: &mut File, elapsed_time: &Duration) {
    if PLAYER_NUM.load(Ordering::Relaxed) >= unsafe { MAX_TEST } as usize {
        if elapsed_time.as_secs() >= 1 {
            writeln!(
                file,
                "{},,",
                unsafe { TOTAL_INTERNAL_DELAY_PER_THREAD[tid].load(Ordering::Relaxed) } as f32
                    / max(1, unsafe {
                        TOTAL_INTERNAL_DELAY_COUNT_PER_THREAD[tid].load(Ordering::Relaxed)
                    }) as f32,
            )
            .unwrap();
            unsafe {
                TOTAL_INTERNAL_DELAY_PER_THREAD[tid].store(0, Ordering::Relaxed);
                TOTAL_INTERNAL_DELAY_COUNT_PER_THREAD[tid].store(0, Ordering::Relaxed);
            }
            TOTAL_EXTERNAL_DELAY_IN_1S.store(0, Ordering::Relaxed);
            TOTAL_EXTERNAL_DELAY_COUNT_IN_1S.store(0, Ordering::Relaxed);
            TOTAL_PACKET_COUNT_IN_1S.store(0, Ordering::Relaxed);
        }
    }
}

fn log_all_delay(file: &mut File, elapsed_time: &Duration, delay_recv: &mut Receiver<isize>) {
    if PLAYER_NUM.load(Ordering::Relaxed) > 0 {
        if elapsed_time.as_secs() >= 1 {
            for delay in delay_recv.try_iter() {
                writeln!(file, "{},,", delay).unwrap();
            }
            TOTAL_EXTERNAL_DELAY_IN_1S.store(0, Ordering::Relaxed);
            TOTAL_INTERNAL_DELAY_IN_1S.store(0, Ordering::Relaxed);
            TOTAL_EXTERNAL_DELAY_COUNT_IN_1S.store(0, Ordering::Relaxed);
            TOTAL_INTERNAL_DELAY_COUNT_IN_1S.store(0, Ordering::Relaxed);
            TOTAL_PACKET_COUNT_IN_1S.store(0, Ordering::Relaxed);
        }
    }
}

use crossterm::event::KeyEvent;
fn blocking_main<F: FnMut(&Duration)>(
    tx: Sender<Event<KeyEvent>>,
    err_recv: Receiver<Error>,
    mut fn_loggin_delay: F,
) {
    let tick_rate = Duration::from_millis(16);
    let mut last_tick = Instant::now();
    let mut elapsed_time_up_to_1s = Duration::default();

    loop {
        if let Some(d_time) = tick_rate.checked_sub(last_tick.elapsed()) {
            if event::poll(d_time).expect("Can't poll event") {
                if let CEvent::Key(key) = event::read().expect("Can't read event") {
                    tx.send(Event::Key(key)).expect("Can't send event message");
                }
            }
            let elapsed_tick = last_tick.elapsed();
            if elapsed_tick > tick_rate {
                elapsed_time_up_to_1s += elapsed_tick;
                fn_loggin_delay(&elapsed_time_up_to_1s);
                tx.send(Event::Tick).expect("Can't send event message");
                last_tick = Instant::now();
            }
        } else {
            elapsed_time_up_to_1s += last_tick.elapsed();
            fn_loggin_delay(&elapsed_time_up_to_1s);
            tx.send(Event::Tick).expect("Can't send event message");
            last_tick = Instant::now();
        }
        if elapsed_time_up_to_1s.as_secs() >= 1 {
            elapsed_time_up_to_1s -= Duration::from_secs(1);
        }
        for err in err_recv.try_iter() {
            tx.send(Event::Err(err)).expect("Can't send event message");
        }

        std::thread::yield_now();
    }
}
