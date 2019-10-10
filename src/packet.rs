use std::mem::size_of;

#[repr(C, packed(1))]
pub struct PacketHeader {
    size: u8,
    p_type: u8,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub enum SCPacketType {
    SC_LOGIN_OK = 1,
    SC_PUT,
    SC_REMOVE,
    SC_POS,
}

impl From<usize> for SCPacketType {
    fn from(value: usize) -> Self {
        let p = match size_of::<Self>() {
            1 => &(value as u8) as *const u8 as *const Self,
            2 => &(value as u16) as *const u16 as *const Self,
            4 => &(value as u32) as *const u32 as *const Self,
            8 => &(value as u64) as *const u64 as *const Self,
            _ => unreachable!(),
        };
        unsafe { *p }
    }
}

enum CSPacketType {
    CS_UP = 1,
    CS_DOWN,
    CS_LEFT,
    CS_RIGHT,
}

#[repr(C, packed(1))]
pub struct SCPutPlayer {
    pub header: PacketHeader,
    pub id: u32,
    pub x: u16,
    pub y: u16,
}
#[repr(C, packed(1))]
pub struct SCPosPlayer {
    pub header: PacketHeader,
    pub id: u32,
    pub x: u16,
    pub y: u16,
}
#[repr(C, packed(1))]
pub struct SCLoginOk {
    pub header: PacketHeader,
    pub id: u32,
}
#[repr(C, packed(1))]
pub struct CSMove(pub PacketHeader);

impl CSMove {
    pub fn left() -> Self {
        Self(PacketHeader {
            size: size_of::<Self>() as u8,
            p_type: CSPacketType::CS_LEFT as u8,
        })
    }
    pub fn right() -> Self {
        Self(PacketHeader {
            size: size_of::<Self>() as u8,
            p_type: CSPacketType::CS_RIGHT as u8,
        })
    }
    pub fn up() -> Self {
        Self(PacketHeader {
            size: size_of::<Self>() as u8,
            p_type: CSPacketType::CS_UP as u8,
        })
    }
    pub fn down() -> Self {
        Self(PacketHeader {
            size: size_of::<Self>() as u8,
            p_type: CSPacketType::CS_DOWN as u8,
        })
    }
}
