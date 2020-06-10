use anyhow::{anyhow, bail, ensure, Context, Error, Result};
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
use std::collections::hash_map::RandomState;
use std::io::{stdout, Write};
use std::str::FromStr;
use std::string::ToString;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicUsize, Ordering};
use std::sync::{
    mpsc::{channel, Sender},
    Arc,
};
use std::time::{Duration, Instant, UNIX_EPOCH};
use structopt::StructOpt;
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

mod packet;

type PlayerMapRead = ReadHandle<i32, Arc<Player>, (), RandomState>;

static mut MAX_TEST: u64 = 0;
static mut BOARD_WIDTH: usize = 0;
static mut BOARD_HEIGHT: usize = 0;
static mut PORT: u16 = 0;
static PLAYER_NUM: AtomicUsize = AtomicUsize::new(0);
static INTERNAL_DELAY: AtomicIsize = AtomicIsize::new(0);
static EXTERNAL_DELAY: AtomicIsize = AtomicIsize::new(0);

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

async fn assemble_packet(
    player: &Player,
    buf: &mut [u8],
    prev_packet_size: &mut usize,
    received_size: usize,
) -> Result<()> {
    let recv_buf = &mut buf[..(*prev_packet_size + received_size)];
    let mut packet_head_idx = 0;

    while packet_head_idx < recv_buf.len() {
        let packet_size = recv_buf[packet_head_idx] as usize;

        if packet_size < 2 {
            bail!("a packet size was less than 2. It was {}", 2);
        }

        let packet_tail_idx = packet_head_idx + packet_size;
        if packet_tail_idx <= recv_buf.len() {
            process_packet(&recv_buf[packet_head_idx..packet_tail_idx], player)?;
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

fn adjust_delay(counter: &AtomicIsize, move_time: u32) {
    use std::time::SystemTime;
    let curr_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u32;
    if curr_time < move_time {
        return;
    }

    let d_ms = (curr_time - move_time) as isize;
    let mut delay = counter.load(Ordering::Relaxed);
    if delay < d_ms {
        counter.fetch_add(d_ms - delay, Ordering::Relaxed);
    } else if delay > d_ms {
        let val = delay - d_ms;
        while delay >= val {
            let prev_delay = counter.compare_and_swap(delay, delay - val, Ordering::Relaxed);
            if prev_delay == delay {
                break;
            } else {
                delay = prev_delay;
            }
        }
    }
}

fn process_packet(packet: &[u8], player: &Player) -> Result<()> {
    use packet::{SCPacketType, SCPosPlayer};
    let packet_type = *packet.get(1).ok_or(anyhow!("packet has no type field"))?;
    match FromPrimitive::from_u8(packet_type) {
        Some(SCPacketType::Pos) => {
            let p = from_bytes::<SCPosPlayer>(packet);
            let id = p.id;

            if player.id == id {
                player.set_pos(p.x as i16, p.y as i16);
                if p.move_time != 0 {
                    adjust_delay(&INTERNAL_DELAY, p.move_time);
                }
            } else if p.move_time != 0 {
                adjust_delay(&EXTERNAL_DELAY, p.move_time);
            }
        }
        _ => {}
    }
    Ok(())
}

async fn read_packet(
    stream: &mut BufReader<&net::TcpStream>,
    player: &Player,
    buf: &mut [u8],
    prev_packet_size: &mut usize,
) -> Result<()> {
    let read_size = stream.read(&mut buf[*prev_packet_size..]).await?;
    ensure!(read_size > 0, "Read zero byte");

    assemble_packet(player, buf, prev_packet_size, read_size).await
}

async fn receiver(stream: Arc<net::TcpStream>, player: Arc<Player>, err_send: Sender<Error>) {
    let mut stream = BufReader::new(&*stream);
    let mut buf = Vec::with_capacity(RECV_SIZE);
    unsafe {
        buf.set_len(RECV_SIZE);
    }
    let mut prev_packet_size = 0usize;
    while player.is_alive.load(Ordering::Relaxed) {
        if let Err(e) = read_packet(
            &mut stream,
            player.as_ref(),
            &mut buf,
            &mut prev_packet_size,
        )
        .await
        {
            err_send.send(e).expect("Can't send error message");
            return;
        }
    }
}

async fn sender(
    stream: Arc<net::TcpStream>,
    player: Arc<Player>,
    err_send: Sender<Error>,
    sleep_time: Duration,
) {
    let mut stream = stream.as_ref();

    // let tele_packet = packet::CSTeleport::new();
    // let p_bytes = unsafe {
    //     std::slice::from_raw_parts(
    //         &tele_packet as *const packet::CSTeleport as *const u8,
    //         std::mem::size_of::<packet::CSTeleport>(),
    //     )
    // };
    // if let Err(e) = stream
    //     .write_all(p_bytes)
    //     .await
    //     .context("Can't send teleport packet")
    // {
    //     err_send.send(e).expect("Can't send error message");
    //     return;
    // }

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
        task::sleep(sleep_time).await;
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
}

enum Event<I> {
    Key(I),
    Tick,
    Err(Error),
}

#[async_std::main]
async fn main() {
    let opt = CmdOption::from_args();
    unsafe {
        MAX_TEST = opt.max_player as u64;
        BOARD_WIDTH = opt.board_width as usize;
        BOARD_HEIGHT = opt.board_height as usize;
        PORT = opt.port;
    }

    let (read_handle, mut write_handle) = Options::default()
        .with_capacity(unsafe { MAX_TEST } as usize)
        .construct();

    let (err_send, err_recv) = channel();

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
        let accept_delay = opt.accept_delay as u128;
        let mut client_to_disconnect = 0;
        let mut max_player_num = unsafe { MAX_TEST };
        let move_cycle = Duration::from_millis(opt.move_cycle);
        let delay_threshold = opt.delay_threshold as usize;
        let delay_threshold2 = (delay_threshold as f64 * 1.5) as usize;
        while PLAYER_NUM.load(Ordering::Relaxed) < unsafe { MAX_TEST } as usize {
            // 접속 가능 여부 판단
            let elapsed_time = last_login_time.elapsed().as_millis();
            if elapsed_time < (accept_delay * delay_multiplier) {
                continue;
            }

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
                    if let None = client
                        .write_all(p_bytes)
                        .await
                        .context("Can't send login packet")
                        .map_err(|e| err_send.send(e))
                        .ok()
                    {
                        continue;
                    }

                    if let Ok(player) = process_login(&mut client, &mut write_handle)
                        .await
                        .map_err(|e| err_send.send(e))
                    {
                        let err_send = err_send.clone();
                        handle = Some(task::spawn(async move {
                            let client = Arc::new(client);
                            let recv = task::spawn(receiver(
                                client.clone(),
                                player.clone(),
                                err_send.clone(),
                            ));
                            let send = task::spawn(sender(client, player, err_send, move_cycle));
                            recv.await;
                            send.await;
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
            for err in err_recv.try_iter() {
                tx.send(Event::Err(err)).expect("Can't send event message");
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
                                            Constraint::Percentage(10),
                                            Constraint::Percentage(80),
                                            Constraint::Percentage(10),
                                        ]
                                        .as_ref(),
                                    )
                                    .split(f.size());
                                let text = [Text::Raw(
                                    format!(
                                        "Internal Delay: {} ms\nExternal Dalay: {} ms\nPlayers: {}",
                                        INTERNAL_DELAY.load(Ordering::Relaxed),
                                        EXTERNAL_DELAY.load(Ordering::Relaxed),
                                        PLAYER_NUM.load(Ordering::Relaxed),
                                    )
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
                                let list = List::new(
                                    err_vec
                                        .iter()
                                        .rev()
                                        .take(10)
                                        .map(|s: &String| Text::Raw(s.as_str().into())),
                                )
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
        })
        .await
    }

    for (id, _) in read_handle.read().iter() {
        disconnect_client(*id, &read_handle);
    }
}
