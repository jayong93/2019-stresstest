use async_std::{
    io::{prelude::*, BufReader},
    net, task,
};
use crossterm::{
    event::{self, Event as CEvent, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use evmap::{self, Options, ReadHandle, WriteHandle};
use num::FromPrimitive;
use rand::prelude::*;
use std::cell::UnsafeCell;
use std::collections::hash_map::RandomState;
use std::io::{stdout, Write};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{mpsc::channel, Arc};
use std::time::{Duration, Instant, UNIX_EPOCH};
use structopt::StructOpt;
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::Color,
    widgets::{
        canvas::{Canvas, Points},
        Block, Borders, Paragraph, Text,
    },
    Terminal,
};

mod packet;

type PlayerMapRead = ReadHandle<i32, Arc<Player>, (), RandomState>;

static mut MAX_TEST: u64 = 0;
static mut WINDOW_SIZE: (usize, usize) = (0, 0);
static mut BOARD_SIZE: usize = 0;
static mut CELL_SIZE: f32 = 0.0;
static mut PORT: u16 = 0;
static PLAYER_NUM: AtomicUsize = AtomicUsize::new(0);
static GLOBAL_DELAY: AtomicUsize = AtomicUsize::new(0);
const DELAY_THRESHOLD: usize = 100;
const DELAY_THRESHOLD_2: usize = 150;

#[derive(Debug)]
struct Player {
    id: i32,
    position: AtomicU32,
    recv_buf: UnsafeCell<Vec<u8>>,
    prev_packet_size: UnsafeCell<u64>,
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
            id: id,
            position: AtomicU32::new(Self::compose_position(x, y)),
            prev_packet_size: UnsafeCell::new(0),
            recv_buf: UnsafeCell::new(vec![0; RECV_SIZE]),
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
) -> Option<Arc<Player>> {
    let mut read_buf = vec![0; 256];
    if let Err(e) = stream.read_exact(&mut read_buf[0..1]).await {
        eprintln!("Can't process login: {}", e);
        return None;
    }

    let total_size = read_buf[0] as usize;
    if let Err(e) = stream.read_exact(&mut read_buf[1..total_size]).await {
        eprintln!("Can't process login: {}", e);
        return None;
    }

    use packet::*;
    match FromPrimitive::from_u8(read_buf[1]) {
        Some(SCPacketType::LoginOk) => {
            let p = from_bytes::<SCLoginOk>(&read_buf[0..total_size]);
            let id = p.id;
            let player = Arc::new(Player::new(id, p.x, p.y));
            write_handle.insert(id, player.clone());
            write_handle.refresh();
            Some(player)
        }
        Some(p) => {
            eprintln!(
                "the server has sent a packet that isn't a login packet. It was {:#?}",
                p
            );
            None
        }
        _ => {
            eprintln!("the server has sent unknown packet type {}", read_buf[1]);
            None
        }
    }
}

fn assemble_packet(player: Arc<Player>, received_size: usize) -> Arc<Player> {
    let recv_buf;
    let prev_size;
    unsafe {
        recv_buf = &mut *player.recv_buf.get();
        prev_size = &mut *player.prev_packet_size.get();
    }
    let mut recv_buf = &mut recv_buf[..received_size];

    while recv_buf.len() > 0 {
        let packet_size = recv_buf[0] as usize;

        if packet_size == 0 {
            eprintln!("a packet size was 0");
            return player;
        }

        let required = packet_size - *prev_size as usize;
        if required <= recv_buf.len() {
            process_packet(&recv_buf[..packet_size], &player);
            recv_buf = &mut recv_buf[packet_size..];
            *prev_size = 0;
        } else {
            unsafe {
                (*player.recv_buf.get())
                    .as_mut_ptr()
                    .copy_from(recv_buf.as_ptr(), recv_buf.len());
            }
            *prev_size = recv_buf.len() as u64;
            break;
        }
    }
    player
}

fn process_packet(packet: &[u8], player: &Arc<Player>) {
    use packet::{SCPacketType, SCPosPlayer};
    match FromPrimitive::from_u8(packet[1]) {
        Some(SCPacketType::Pos) => {
            let p = from_bytes::<SCPosPlayer>(packet);
            let id = p.id;

            if player.id == id {
                player.set_pos(p.x as i16, p.y as i16);
                if p.move_time != 0 {
                    let d_ms = UNIX_EPOCH.elapsed().unwrap().as_millis() as u32 - p.move_time;
                    let global_delay = GLOBAL_DELAY.load(Ordering::Relaxed);
                    if global_delay < d_ms as usize {
                        GLOBAL_DELAY.fetch_add(1, Ordering::Relaxed);
                    } else if global_delay > d_ms as usize {
                        if GLOBAL_DELAY.fetch_sub(1, Ordering::Relaxed) == 0 {
                            GLOBAL_DELAY.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

async fn read_packet(
    stream: &mut BufReader<&net::TcpStream>,
    player: Arc<Player>,
) -> Result<Arc<Player>, ()> {
    let read_buf;
    {
        let read_ptr = player.recv_buf.get();
        read_buf = unsafe {
            &mut ((&mut *read_ptr).as_mut_slice())[*player.prev_packet_size.get() as usize..]
        };
    }
    let read_size = stream
        .read(read_buf)
        .await
        .map_err(|e| eprintln!("{}", e))?;
    if read_size == 0 {
        eprintln!("Read zero byte");
        return Err(());
    }

    Ok(task::spawn_blocking(move || assemble_packet(player, read_size)).await)
}

async fn receiver(stream: Arc<net::TcpStream>, mut player: Arc<Player>) {
    let mut stream = BufReader::new(&*stream);
    while player.is_alive.load(Ordering::Relaxed) {
        if let Ok(p) = read_packet(&mut stream, player).await {
            player = p
        } else {
            eprintln!("Error occured in read_packet");
            return;
        }
    }
}

async fn sender(stream: Arc<net::TcpStream>, player: Arc<Player>) {
    let mut stream = stream.as_ref();

    let tele_packet = packet::CSTeleport::new();
    let p_bytes = unsafe {
        std::slice::from_raw_parts(
            &tele_packet as *const packet::CSTeleport as *const u8,
            std::mem::size_of::<packet::CSTeleport>(),
        )
    };
    stream
        .write_all(p_bytes)
        .await
        .expect("Can't send teleport packet");

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
        if stream.write_all(bytes).await.is_err() {
            eprintln!("Can't send to the server");
            return;
        }
        task::sleep(Duration::from_millis(1000)).await;
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
    #[structopt(short, long, default_value = "400")]
    board_size: u16,

    #[structopt(short, long, default_value = "1000")]
    max_player: usize,

    #[structopt(short, long, default_value = "800")]
    window_size: Vec<usize>,

    #[structopt(short, long, default_value = "9000")]
    port: u16,

    #[structopt(short, long, default_value = "127.0.0.1")]
    ip_addr: String,
}

enum Event<I> {
    Key(I),
    Tick,
}

#[async_std::main]
async fn main() {
    let opt = CmdOption::from_args();
    unsafe {
        MAX_TEST = opt.max_player as u64;
        match opt.window_size.len() {
            1 => {
                let size = opt.window_size.first().unwrap();
                WINDOW_SIZE = (*size, *size);
            }
            _ => {
                WINDOW_SIZE = (opt.window_size[0], opt.window_size[1]);
            }
        }
        BOARD_SIZE = opt.board_size as usize;
        PORT = opt.port;
        CELL_SIZE = (WINDOW_SIZE.0 as f32) / (BOARD_SIZE as f32);
    }

    let (read_handle, mut write_handle) = Options::default()
        .with_capacity(unsafe { MAX_TEST } as usize)
        .construct();

    let server = async move {
        let mut handle = None;
        let mut ip_addr =
            std::net::SocketAddrV4::new(FromStr::from_str(opt.ip_addr.as_str()).unwrap(), unsafe {
                PORT
            });
        ip_addr.set_port(unsafe { PORT });

        let mut last_login_time = Instant::now();
        let mut delay_multiplier = 1;
        let mut is_increasing = true;
        let accept_delay = 50;
        let mut client_to_disconnect = 0;
        let mut max_player_num = unsafe { MAX_TEST };
        while PLAYER_NUM.load(Ordering::Relaxed) < unsafe { MAX_TEST } as usize {
            // 접속 가능 여부 판단
            let elapsed_time = last_login_time.elapsed().as_millis();
            if elapsed_time < (accept_delay as u128 * delay_multiplier) {
                continue;
            }

            let g_delay = GLOBAL_DELAY.load(Ordering::Relaxed);
            let cur_player_num = PLAYER_NUM.load(Ordering::Relaxed) as u64;
            if DELAY_THRESHOLD_2 < g_delay {
                if is_increasing {
                    max_player_num = cur_player_num;
                    is_increasing = false;
                }
                if cur_player_num < 100 {
                    continue;
                }
                if elapsed_time < (accept_delay * 2) as u128 {
                    continue;
                }

                last_login_time = Instant::now();
                disconnect_client(client_to_disconnect, &mut write_handle);
                client_to_disconnect += 1;
                continue;
            } else if DELAY_THRESHOLD < g_delay {
                delay_multiplier = 2;
                continue;
            }

            if max_player_num != unsafe { MAX_TEST }
                && max_player_num - (max_player_num / 20) < cur_player_num
            {
                continue;
            }

            is_increasing = true;
            last_login_time = Instant::now();

            match net::TcpStream::connect(ip_addr).await {
                Ok(mut client) => {
                    client.set_nodelay(true).ok();

                    let login_packet = packet::CSLogin::new();
                    let p_bytes = unsafe {
                        std::slice::from_raw_parts(
                            &login_packet as *const packet::CSLogin as *const u8,
                            std::mem::size_of::<packet::CSLogin>(),
                        )
                    };
                    client
                        .write_all(p_bytes)
                        .await
                        .expect("Can't send login packet");

                    if let Some(player) = process_login(&mut client, &mut write_handle).await {
                        handle = Some(task::spawn(async move {
                            let client = Arc::new(client);
                            let recv = task::spawn(receiver(client.clone(), player.clone()));
                            let send = task::spawn(sender(client, player));
                            recv.await;
                            send.await;
                        }));
                        PLAYER_NUM.fetch_add(1, Ordering::Relaxed);
                    } else {
                        eprintln!("Can't process login");
                        continue;
                    }
                }
                Err(e) => eprintln!("Can't connect to server: {}", e),
            }
            // 접속 시도
        }
        handle.unwrap().await;
    };
    task::spawn(server);

    let tick_rate = Duration::from_millis(16);

    let (tx, rx) = channel();

    task::spawn_blocking(move || {
        let mut last_tick = Instant::now();
        loop {
            if let Some(d_time) = tick_rate.checked_sub(last_tick.elapsed()) {
                if event::poll(d_time).expect("Can't poll event") {
                    if let CEvent::Key(key) = event::read().expect("Can't read event") {
                        tx.send(Event::Key(key)).expect("Can't send event message");
                    }
                }
                if last_tick.elapsed() > tick_rate {
                    tx.send(Event::Tick).expect("Can't send event message");
                    last_tick = Instant::now();
                }
            } else {
                tx.send(Event::Tick).expect("Can't send event message");
                last_tick = Instant::now();
            }
            std::thread::yield_now();
        }
    });

    {
        let read_handle = read_handle.clone();
        task::spawn_blocking(move || {
            enable_raw_mode().expect("Can't use raw mode");
            let mut stdout = stdout();
            execute!(stdout, EnterAlternateScreen).expect("Can't enter to alternate screen");
            let backend = CrosstermBackend::new(stdout);
            let mut terminal = Terminal::new(backend).expect("Can't create a terminal");
            terminal.hide_cursor().expect("Can't hide a cursor");

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
                                        [Constraint::Percentage(10), Constraint::Percentage(90)]
                                            .as_ref(),
                                    )
                                    .split(f.size());
                                let text = [
                                    Text::Raw(
                                        format!(
                                            "Delay: {} ms, ",
                                            GLOBAL_DELAY.load(Ordering::Relaxed)
                                        )
                                        .into(),
                                    ),
                                    Text::Raw(
                                        format!(
                                            "Players: {}\n",
                                            PLAYER_NUM.load(Ordering::Relaxed)
                                        )
                                        .into(),
                                    ),
                                ];
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
                                    .x_bounds([0.0, unsafe { BOARD_SIZE as f64 }])
                                    .y_bounds([0.0, unsafe { BOARD_SIZE as f64 }]);
                                f.render_widget(canvas, layout[1]);
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
                    _ => {}
                }
            }
        })
        .await
    }

    for (id, _) in read_handle.read().iter() {
        disconnect_client(*id, &read_handle);
    }
}
