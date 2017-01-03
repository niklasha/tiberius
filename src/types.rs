///! type converting, mostly translating the types received from the database into rust types
use std::borrow::Cow;
use std::cmp;
use std::io::Cursor;
use byteorder::{ByteOrder, LittleEndian, ReadBytesExt, WriteBytesExt};
use futures::{Async, Poll};
use tokio_core::io::Io;
use tokens::BaseMetaDataColumn;
use transport::{self, TdsBuf, TdsTransport};
use {FromUint, TdsResult, TdsError};

#[derive(Copy, Clone, Debug)]
#[repr(u8)]
pub enum FixedLenType {
    Null        = 0x1F,
    Int1        = 0x30,
    Bit         = 0x32,
    Int2        = 0x34,
    Int4        = 0x38,
    Datetime4   = 0x3A,
    Float4      = 0x3B,
    Money       = 0x3C,
    Datetime    = 0x3D,
    Float8      = 0x3E,
    Money4      = 0x7A,
    Int8        = 0x7F
}
uint_to_enum!(FixedLenType, Null, Int1, Bit, Int2, Int4, Datetime4, Float4, Money, Datetime, Float8, Money4, Int8);

/// 2.2.5.4.2
#[derive(Copy, Clone, Debug)]
#[repr(u8)]
pub enum VarLenType {
    Guid = 0x24,
    Intn = 0x26,
    Bitn = 0x68,
    Decimaln = 0x6A,
    Numericn = 0x6C,
    Floatn = 0x6D,
    Money = 0x6E,
    Datetimen = 0x6F,
    /// introduced in TDS 7.3
    Daten = 0x28,
    /// introduced in TDS 7.3
    Timen = 0x29,
    /// introduced in TDS 7.3
    Datetime2 = 0x2A,
    /// introduced in TDS 7.3
    DatetimeOffsetn = 0x2B,
    BigVarBin = 0xA5,
    BigVarChar = 0xA7,
    BigBinary = 0xAD,
    BigChar = 0xAF,
    NVarchar = 0xE7,
    NChar = 0xEF,
    // not supported yet
    Xml = 0xF1,
    // not supported yet
    Udt = 0xF0,
    Text = 0x23,
    Image = 0x22,
    NText = 0x63,
    // not supported yet
    SSVariant = 0x62
    // legacy types (not supported since post-7.2):
    // Char = 0x2F,
    // VarChar = 0x27,
    // Binary = 0x2D,
    // VarBinary = 0x25,
    // Numeric = 0x3F,
    // Decimal = 0x37,
}
uint_to_enum!(VarLenType, Guid, Intn, Bitn, Decimaln, Numericn, Floatn, Money, Datetimen, Daten, Timen, Datetime2, DatetimeOffsetn,
    BigVarBin, BigVarChar, BigBinary, BigChar, NVarchar, NChar, Xml, Udt, Text, Image, NText, SSVariant);

#[derive(Clone, Debug)]
pub enum TypeInfo {
    FixedLen(FixedLenType),
    VarLenSized(VarLenType, usize)
}

#[derive(Debug)]
pub enum ColumnData<'a> {
    I32(i32),
    /// owned/borrowed rust string
    String(Cow<'a, str>),
    /// a buffer string which is a reference to a buffer of a received packet
    BString(TdsBuf),
}

impl TypeInfo {
    pub fn parse<I: Io>(trans: &mut TdsTransport<I>) -> Poll<TypeInfo, TdsError> {
        let ty = try!(trans.read_u8());
        if let Some(ty) = FixedLenType::from_u8(ty) {
            return Ok(Async::Ready(TypeInfo::FixedLen(ty)))
        }
        if let Some(ty) = VarLenType::from_u8(ty) {
            let vty = match ty {
                VarLenType::Intn => TypeInfo::VarLenSized(ty, try!(trans.read_u8()) as usize),
                _ => unimplemented!()
            };
            return Ok(Async::Ready(vty))
        }
        return Err(TdsError::Protocol(format!("invalid or unsupported column type: {:?}", ty).into()))
    }
}

impl<'a> ColumnData<'a> {
    pub fn parse<I: Io>(trans: &mut TdsTransport<I>, meta: &BaseMetaDataColumn) -> TdsResult<ColumnData<'a>> {
        Ok(match meta.ty {
            TypeInfo::FixedLen(ref fixed_ty) => {
                match *fixed_ty {
                    FixedLenType::Int4 => ColumnData::I32(try!(trans.read_i32::<LittleEndian>())),
                    _ => panic!("unsupported fixed type decoding: {:?}", fixed_ty)
                }
            },
            TypeInfo::VarLenSized(ref ty, ref len) => {
                match (*ty, *len) {
                    (VarLenType::Intn, 4) => {
                        assert_eq!(try!(trans.read_u8()), 4);
                        ColumnData::I32(try!(trans.read_i32::<LittleEndian>()))
                    },
                    _ => unimplemented!()
                }
            },
        })
    }

    pub fn serialize(&self, target: &mut Cursor<Vec<u8>>, last_pos: usize) -> TdsResult<Option<usize>> {
        match *self {
            ColumnData::I32(ref val) => {
                // write progressively
                let mut bytes = [VarLenType::Intn as u8, 4, 4, 0, 0, 0, 0];
                LittleEndian::write_i32(&mut bytes[3..], *val as i32);
                let (left_bytes, written_bytes) = try!(transport::write_bytes_fragment(target, &bytes, last_pos));
                if left_bytes > 0 {
                    return Ok(Some(last_pos + written_bytes))
                }
            },
            ColumnData::String(ref str_) => {
                // type
                if last_pos == 0 {
                    // TODO: for a certain size we need to send it as BIGNVARCHAR (?)...
                    try!(target.write_u8(VarLenType::NVarchar as u8)); // pos:0
                }
                let mut state = cmp::max(last_pos, 1);
                // type length
                if state < 3 {
                    let length = 2*str_.len();
                    assert!(length < u16::max_value() as usize);
                    let (left_bytes, written_bytes) = try!(transport::write_u16_fragment::<LittleEndian>(target, length as u16, state - 1));
                    if left_bytes > 0 {
                        return Ok(Some(state + written_bytes))
                    }
                    state = 3;
                }
                // collation
                if state < 8 {
                    // TODO: DO NOT USE A HARDCODED (AND PROBABLY INVALID) COLLATION
                    let collation = [0u8, 0, 0, 0, 0]; // pos: [3,4,5,6,7]
                    let (left_bytes, written_bytes) = try!(transport::write_bytes_fragment(target, &collation, state - 3));
                    if left_bytes > 0 {
                        return Ok(Some(state + written_bytes))
                    }
                    state = 8;
                }
                // body length
                if state < 10 {
                    let (left_bytes, written_bytes) = try!(transport::write_u16_fragment::<LittleEndian>(target, 2*str_.len() as u16, state - 8));
                    if left_bytes > 0 {
                        return Ok(Some(state + written_bytes))
                    }
                    state = 10;
                }
                // encoded string pos:>=8
                if state >= 10 {
                    let (left_bytes, written_bytes) = try!(transport::write_varchar_fragment(target, str_, state - 10));
                    if left_bytes > 0 {
                        return Ok(Some(state + written_bytes))
                    }
                }
            },
            _ => unimplemented!()
        }
        Ok(None)
    }
}

pub trait FromColumnData: Sized {
    fn from_column_data(data: &ColumnData) -> TdsResult<Self>;
}

impl FromColumnData for i32 {
    fn from_column_data(data: &ColumnData) -> TdsResult<i32> {
        match *data {
            ColumnData::I32(value) => Ok(value),
            _ => Err(TdsError::Conversion("cannot interpret the given column data as an i32 value".into()))
        }
    }
}

pub trait ToColumnData {
    fn to_column_data(&self) -> ColumnData;
}

impl ToColumnData for i32 {
    fn to_column_data(&self) -> ColumnData {
        ColumnData::I32(*self)
    }
}
