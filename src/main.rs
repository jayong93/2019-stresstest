use async_std::{
    io::{prelude::*, BufReader},
    net,
};
use evmap::{self, Options, ReadHandle, WriteHandle};
use ggez::conf::{WindowMode, WindowSetup};
use ggez::event::{self, EventHandler};
use ggez::graphics;
use ggez::graphics::Drawable;
use ggez::{Context, ContextBuilder, GameResult};
use rand::prelude::*;
use std::collections::hash_map::RandomState;
use std::str::FromStr;
use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use structopt::StructOpt;
use tokio::{task, time};

mod packet;

type PlayerMapRead = ReadHandle<i32, Arc<Player>, (), RandomState>;

static mut MAX_TEST: u64 = 0;
static mut WINDOW_SIZE: (usize, usize) = (0, 0);
static mut BOARD_SIZE: usize = 0;
static mut CELL_SIZE: f32 = 0.0;
static mut PORT: u16 = 0;
static PLAYER_NUM: AtomicUsize = AtomicUsize::new(0);
static GLOBAL_DELAY: AtomicUsize = AtomicUsize::new(0);
const DELAY_THRESHOLD: usize = 1000;

struct Player {
    _id: i32,
    position: AtomicU32,
    seq_no: AtomicI32,
    last_move_ptr: AtomicPtr<Instant>,
    last_move_time: Box<Instant>,
}

impl Drop for Player {
    fn drop(&mut self) {
        unsafe { Box::from_raw(self.last_move_ptr.load(Ordering::Relaxed)) };
    }
}

impl PartialEq for Player {
    fn eq(&self, other: &Self) -> bool {
        self._id == other._id
    }
}

impl Eq for Player {}

impl std::hash::Hash for Player {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self._id.hash(state);
    }
}

impl Player {
    fn new(id: i32, x: i16, y: i16) -> Self {
        let now = Instant::now();
        Player {
            _id: id,
            position: AtomicU32::new(Self::compose_position(x, y)),
            seq_no: AtomicI32::new(0),
            last_move_ptr: AtomicPtr::new(std::ptr::null_mut()),
            last_move_time: Box::new(now),
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

    fn change_move_time(&self, new_time: Option<Box<Instant>>) -> Option<Box<Instant>> {
        let new_ptr = if let Some(p) = new_time {
            Box::into_raw(p)
        } else {
            std::ptr::null_mut()
        };

        let mut old_ptr = self.last_move_ptr.load(Ordering::Relaxed);
        while let Err(ptr) = self.last_move_ptr.compare_exchange(
            old_ptr,
            new_ptr,
            Ordering::SeqCst,
            Ordering::Relaxed,
        ) {
            old_ptr = ptr;
        }

        if old_ptr.is_null() {
            None
        } else {
            Some(unsafe{Box::from_raw(old_ptr)})
        }
    }

    unsafe fn set_move_time(&self, new_time: Box<Instant>) {
        (*(self as *const Self as *mut Self)).last_move_time = new_time;
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
) -> Option<(i32, Arc<Player>)> {
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
            let p = from_bytes::<SCLoginOk>(&read_buf);
            let id = p.id;
            let player = Arc::new(Player::new(id, p.x, p.y));
            write_handle.insert(id, player.clone());
            write_handle.refresh();
            Some((id, player))
        }
        _ => {
            eprintln!("the server has sent a packet that isn't a login packet");
            None
        }
    }
}

async fn process_packet(
    packet: &[u8],
    my_id: i32,
    map_reader: PlayerMapRead,
) {
    use packet::*;
    match SCPacketType::from(packet[1] as usize) {
        SCPacketType::Pos => {
            let p = from_bytes::<SCPosPlayer>(packet);
            let id = p.id;

            if my_id == id {
                if let Some(it) = map_reader.get(&id) {
                    if let Some(player) = it.iter().next() {
                        player.set_pos(p.x as i16, p.y as i16);
                        if p.seq_no > 10 {
                            if let Some(time) = player.change_move_time(None) {
                                // receiver에서만 사용하므로 안전
                                unsafe { player.set_move_time(time) }
                            }
                            let last_time = *player.last_move_time;
                            let now_inst = Instant::now();
                            let mut d_ms = now_inst.duration_since(last_time).as_millis();
                            if p.seq_no != player.seq_no.load(Ordering::Relaxed) {
                                d_ms = std::cmp::max(DELAY_THRESHOLD as u128, d_ms);
                            }
                            let g_delay = GLOBAL_DELAY.load(Ordering::Relaxed);
                            if (g_delay as u128) < d_ms {
                                GLOBAL_DELAY.fetch_add(1, Ordering::Relaxed);
                            } else if (g_delay as u128) > d_ms {
                                let g_delay = GLOBAL_DELAY.fetch_sub(1, Ordering::Relaxed);
                                if g_delay == 0 {
                                    GLOBAL_DELAY.store(0, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

async fn read_packet(
    read_buf: &mut Vec<u8>,
    stream: &mut BufReader<&net::TcpStream>,
    my_id: i32,
    map_reader: PlayerMapRead,
) {
    let read_result = stream.read_exact(&mut read_buf[..1]).await;
    if read_result.is_err() {
        return;
    }
    let total_size = read_buf[0] as usize;
    if stream
        .read_exact(&mut read_buf[1..total_size])
        .await
        .is_err()
    {
        return;
    }
    process_packet(&read_buf[..total_size], my_id, map_reader).await;
}

async fn receiver(
    stream: Arc<net::TcpStream>,
    my_id: i32,
    map_reader: PlayerMapRead,
) {
    let mut stream = BufReader::new(&*stream);
    let mut read_buf = vec![0; 256];
{
    let map_reader = map_reader.clone();
    read_packet(&mut read_buf, &mut stream, my_id, map_reader).await;
}
    loop {
        let map_reader = map_reader.clone();
        read_packet(&mut read_buf, &mut stream, my_id, map_reader).await;
    }
}

async fn sender(stream: Arc<net::TcpStream>, player: Arc<Player>) {
    let mut stream = &*stream;

    let tele_packet = packet::CSTeleport::new();
    let p_bytes = unsafe {
        std::slice::from_raw_parts(
            &tele_packet as *const packet::CSTeleport as *const u8,
            std::mem::size_of::<packet::CSTeleport>(),
        )
    };
    stream.write_all(p_bytes).await.unwrap();

    let mut move_time: Option<Box<Instant>> = None;

    let mut rng = StdRng::from_entropy();
    loop {
        let seq_no = player.seq_no.fetch_add(1, Ordering::Relaxed) + 1;
        let packets = [
            packet::CSMove::new(packet::Direction::Up, seq_no),
            packet::CSMove::new(packet::Direction::Down, seq_no),
            packet::CSMove::new(packet::Direction::Left, seq_no),
            packet::CSMove::new(packet::Direction::Right, seq_no),
        ];
        let picked_packet = &packets[rng.gen_range(0, packets.len())];
        let bytes = unsafe {
            std::slice::from_raw_parts(
                picked_packet as *const packet::CSMove as *const u8,
                std::mem::size_of::<packet::CSMove>(),
            )
        };
        let time = if let Some(mut time) = move_time {
            *time = Instant::now();
            time
        } else {
            Box::new(Instant::now())
        };
        move_time = player.change_move_time(Some(time));
        if stream.write_all(bytes).await.is_err() {
            eprintln!("Can't send to the server");
            return;
        }
        time::delay_for(Duration::from_millis(1000)).await;
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
                "Delay: {} ms, Player: {}",
                GLOBAL_DELAY.load(Ordering::Relaxed),
                PLAYER_NUM.load(Ordering::Relaxed)
            );
            graphics::Text::new(delay_s).draw(ctx, graphics::DrawParam::new())?;
        }
        graphics::present(ctx)
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

fn main() -> GameResult {
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
    let read_handle_clone = read_handle.clone();

    let server = async move {
        let mut handle = None;
        let mut ip_addr =
            std::net::SocketAddrV4::new(FromStr::from_str(opt.ip_addr.as_str()).unwrap(), unsafe {
                PORT
            });
        ip_addr.set_port(unsafe { PORT });

        let mut rng = StdRng::from_entropy();
        while PLAYER_NUM.load(Ordering::Relaxed) < unsafe { MAX_TEST } as usize {
            let mut delay = 50;
            let g_delay = GLOBAL_DELAY.load(Ordering::Relaxed) as u64;

            if g_delay < DELAY_THRESHOLD as u64 {
                if g_delay > 50 {
                    delay = rng.gen_range(50, g_delay);
                }

                if let Ok(mut client) = net::TcpStream::connect(ip_addr).await {
                    client.set_nodelay(true).ok();

                    let login_packet = packet::CSLogin::new();
                    let p_bytes = unsafe {
                        std::slice::from_raw_parts(
                            &login_packet as *const packet::CSLogin as *const u8,
                            std::mem::size_of::<packet::CSLogin>(),
                        )
                    };
                    client.write_all(p_bytes).await.unwrap();

                    let (id, player) = process_login(&mut client, &mut write_handle).await.unwrap();

                    let read_handle = read_handle.clone();
                    handle = Some(task::spawn(async move {
                        let client = Arc::new(client);
                        let recv =
                            task::spawn(receiver(client.clone(), id, read_handle));
                        let send = task::spawn(sender(client, player));
                        PLAYER_NUM.fetch_add(1, Ordering::Relaxed);
                        recv.await.unwrap();
                        send.await.unwrap();
                    }));
                }
            }
            time::delay_for(Duration::from_millis(delay)).await;
        }
        handle.unwrap().await.unwrap();
    };

    let setup = WindowSetup::default().title("Stress Test");
    let win_mode =
        unsafe { WindowMode::default().dimensions(WINDOW_SIZE.0 as f32, WINDOW_SIZE.1 as f32) };
    let cb = ContextBuilder::new("PL-StressTest", "CJY")
        .window_setup(setup)
        .window_mode(win_mode);
    let (mut ctx, mut event_loop) = cb.build()?;
    let mut game_state = GameState {
        player_read_handle: read_handle_clone,
    };

    let runtime = tokio::runtime::Builder::default()
        .threaded_scheduler()
        .enable_all()
        .build()
        .unwrap();
    runtime.spawn(server);
    event::run(&mut ctx, &mut event_loop, &mut game_state)
}
