use async_std::{
    io::{prelude::*, BufReader},
    net, task,
};
use evmap::{self, Options, ReadHandle, WriteHandle};
use ggez::conf::{WindowMode, WindowSetup};
use ggez::event::{self, EventHandler};
use ggez::graphics;
use ggez::graphics::Drawable;
use ggez::{Context, ContextBuilder, GameResult};
use rand::prelude::*;
use std::cell::UnsafeCell;
use std::collections::hash_map::RandomState;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};
use structopt::StructOpt;

mod packet;

type PlayerMapRead = ReadHandle<i32, Arc<Player>, (), RandomState>;
type PlayerMapWrite = WriteHandle<i32, Arc<Player>, (), RandomState>;

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
    packet_buf: UnsafeCell<Vec<u8>>,
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
            packet_buf: UnsafeCell::new(Vec::with_capacity(RECV_SIZE * 2)),
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
    match SCPacketType::from(read_buf[1] as usize) {
        SCPacketType::LoginOk => {
            let p = from_bytes::<SCLoginOk>(&read_buf[0..total_size]);
            let id = p.id;
            let player = Arc::new(Player::new(id, p.x, p.y));
            write_handle.insert(id, player.clone());
            write_handle.refresh();
            Some(player)
        }
        _ => {
            eprintln!("the server has sent a packet that isn't a login packet");
            None
        }
    }
}

fn assemble_packet(player: Arc<Player>, mut received_size: usize) -> Arc<Player> {
    unsafe {
        let recv_buf = &mut *player.recv_buf.get();
        let prev_buf = &mut *player.packet_buf.get();
        let mut packet = [0u8; 256];
        let mut recv_buf = &mut recv_buf[..];

        while received_size > 0 {
            let prev_len = prev_buf.len();
            let packet_size = if prev_len > 0 {
                prev_buf[0]
            } else {
                recv_buf[0]
            } as usize;

            if prev_len + received_size >= packet_size {
                let copied_size = packet_size - prev_len;
                packet[..prev_len].copy_from_slice(prev_buf);
                packet[prev_len..packet_size].copy_from_slice(&recv_buf[..copied_size]);
                recv_buf = &mut recv_buf[copied_size..];
                prev_buf.clear();
                received_size -= copied_size;

                process_packet(&packet[..packet_size], &player);
            } else {
                prev_buf.reserve(prev_len + received_size);
                prev_buf.set_len(prev_len + received_size);
                prev_buf[prev_len..].copy_from_slice(&recv_buf[..received_size]);
                break;
            }
        }
    }
    player
}

fn process_packet(packet: &[u8], player: &Arc<Player>) {
    use packet::{SCPacketType, SCPosPlayer};
    match SCPacketType::from(packet[1] as usize) {
        SCPacketType::Pos => {
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
        read_buf = unsafe { &mut *read_ptr };
    }
    let read_size = stream
        .read(read_buf.as_mut_slice())
        .await
        .map_err(|e| eprintln!("{}", e))?;
    if read_size == 0 {
        eprintln!("Read zero byte");
        return Err(());
    }
    let total_size = read_buf[0] as usize;
    if total_size <= 0 {
        eprintln!("a packet has 0 size");
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
    stream.write_all(p_bytes).await.unwrap();

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

struct GameState {
    player_read_handle: PlayerMapRead,
}

impl EventHandler for GameState {
    fn update(&mut self, ctx: &mut Context) -> GameResult {
        let delta = ggez::timer::delta(ctx).as_secs_f32();
        let target = 1.0 / 30.0;
        if delta <= 1.0 / 30.0 {
            ggez::timer::sleep(std::time::Duration::from_secs_f32(target - delta));
        }
        Ok(())
    }

    fn draw(&mut self, ctx: &mut Context) -> GameResult {
        graphics::clear(ctx, [0.0, 0.0, 0.0, 1.0].into());
        let mut mesh_builder = graphics::MeshBuilder::new();
        let d_mode = graphics::DrawMode::fill();
        let can_render;
        {
            can_render = !self.player_read_handle.is_empty();

            for (_, players) in self.player_read_handle.read().iter() {
                if let Some(player) = players.iter().next() {
                    if !player.is_alive.load(Ordering::Relaxed) {
                        continue;
                    }
                    let (x, y) = player.get_pos();
                    unsafe {
                        let cell_x: f32 = x as f32 * CELL_SIZE + (CELL_SIZE / 4.0);
                        let cell_y: f32 = y as f32 * CELL_SIZE + (CELL_SIZE / 4.0);
                        mesh_builder.rectangle(
                            d_mode,
                            [cell_x, cell_y, CELL_SIZE / 2.0, CELL_SIZE / 2.0].into(),
                            [1.0, 1.0, 1.0, 1.0].into(),
                        );
                    }
                }
            }
        }
        if can_render {
            mesh_builder
                .build(ctx)?
                .draw(ctx, graphics::DrawParam::new())?;

            let delay_s = format!(
                "Delay: {} ms\nPlayer: {}",
                GLOBAL_DELAY.load(Ordering::Relaxed),
                PLAYER_NUM.load(Ordering::Relaxed)
            );
            graphics::Text::new(delay_s).draw(ctx, graphics::DrawParam::new())?;
        }
        graphics::present(ctx)
    }
}

fn disconnect_client(client_id: i32, write_handle: &mut PlayerMapWrite) {
    if let Some(values) = write_handle.get(&client_id) {
        let player = values.iter().next().unwrap();
        player.is_alive.store(false, Ordering::Relaxed);
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

#[async_std::main]
async fn main() -> GameResult {
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
                    unsafe {
                        MAX_TEST = cur_player_num;
                    }
                    is_increasing = false;
                }
                if cur_player_num < 100 {
                    continue;
                }
                if elapsed_time < (accept_delay * 10) as u128 {
                    continue;
                }

                last_login_time = Instant::now();
                disconnect_client(client_to_disconnect, &mut write_handle);
                client_to_disconnect += 1;
                continue;
            } else if DELAY_THRESHOLD < g_delay {
                delay_multiplier = 10;
                continue;
            }

            unsafe {
                if MAX_TEST - (MAX_TEST / 20) < cur_player_num {
                    continue;
                }
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
                    client.write_all(p_bytes).await.unwrap();

                    let player = process_login(&mut client, &mut write_handle).await.unwrap();

                    handle = Some(task::spawn(async move {
                        let client = Arc::new(client);
                        let recv = task::spawn(receiver(client.clone(), player.clone()));
                        let send = task::spawn(sender(client, player));
                        PLAYER_NUM.fetch_add(1, Ordering::Relaxed);
                        recv.await;
                        send.await;
                    }));
                }
                Err(e) => eprintln!("Can't connect to server: {}", e),
            }
            // 접속 시도
        }
        handle.unwrap().await;
    };
    task::spawn(server);

    task::spawn_blocking(|| {
        let setup = WindowSetup::default().title("Stress Test");
        let win_mode =
            unsafe { WindowMode::default().dimensions(WINDOW_SIZE.0 as f32, WINDOW_SIZE.1 as f32) };
        let cb = ContextBuilder::new("PL-StressTest", "CJY")
            .window_setup(setup)
            .window_mode(win_mode);
        let (mut ctx, mut event_loop) = cb.build()?;
        let mut game_state = GameState {
            player_read_handle: read_handle,
        };
        event::run(&mut ctx, &mut event_loop, &mut game_state)
    })
    .await
}
