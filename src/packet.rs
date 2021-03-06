use std::mem::size_of;
use num_derive::FromPrimitive;

#[repr(C, packed(1))]
pub struct PacketHeader {
    size: i8,
    p_type: i8,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, FromPrimitive)]
#[allow(dead_code)]
pub enum SCPacketType {
    LoginOk = 1,
    LoginFail,
    Pos,
    Put,
    Remove,
    Chat,
}

#[repr(C)]
#[derive(Copy, Clone)]
#[allow(dead_code)]
enum CSPacketType {
    Login = 1,
    Move,
    Attack,
    Chat,
    Logout,
    Teleport,
}

#[repr(C, packed(1))]
#[allow(dead_code)]
pub struct SCPutPlayer {
    pub header: PacketHeader,
    pub id: i32,
    pub o_type: i8,
    pub x: i16,
    pub y: i16,
}
#[repr(C, packed(1))]
pub struct SCPosPlayer {
    pub header: PacketHeader,
    pub id: i32,
    pub x: i16,
    pub y: i16,
    pub move_time: u32,
}
#[repr(C, packed(1))]
pub struct SCLoginOk {
    pub header: PacketHeader,
    pub id: i32,
    pub x: i16,
    pub y: i16,
    pub hp: i16,
    pub level: i16,
    pub exp: i32,
}
#[repr(C, packed(1))]
pub struct CSMove {
    pub header: PacketHeader,
    pub direction: i8,
    pub move_time: u32,
}
#[repr(C, packed(1))]
pub struct CSLogin {
    pub header: PacketHeader,
    pub id: [i8; 50],
}
#[repr(C, packed(1))]
pub struct CSTeleport {
    pub header: PacketHeader,
}

#[repr(C)]
pub enum Direction {
    Up = 0,
    Down,
    Left,
    Right,
}

impl CSMove {
    pub fn new(direction: Direction, move_time: u32) -> Self {
        Self {
            header: PacketHeader {
                size: size_of::<Self>() as i8,
                p_type: CSPacketType::Move as i8,
            },
            direction: direction as i8,
            move_time,
        }
    }
}

impl CSLogin {
    pub fn new() -> Self {
        Self {
            header: PacketHeader {
                size: size_of::<Self>() as i8,
                p_type: CSPacketType::Login as i8,
            },
            id: [0i8; 50],
        }
    }
}

impl CSTeleport {
    pub fn new() -> Self {
        Self {
            header: PacketHeader {
                size: size_of::<Self>() as i8,
                p_type: CSPacketType::Teleport as i8,
            },
        }
    }
}
