use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::openrs2::js5::Cursor;

#[derive(Debug, Clone)]
pub enum ParamValue {
    Int(i32),
    #[allow(dead_code)]
    String(String),
}

impl ParamValue {
    pub fn as_i32(&self) -> Option<i32> {
        match self {
            ParamValue::Int(v) => Some(*v),
            ParamValue::String(_) => None,
        }
    }
}

/// Decoded RS3 Archive 19 item definition (ObjType) supporting modern NXT builds (900+).
#[derive(Debug, Clone, Default)]
pub struct ItemDef {
    pub id: u32,
    pub name: Option<String>,
    pub buff_effect: Option<String>,
    pub stackable: bool,
    pub never_stackable: bool,
    pub value: Option<i64>,
    pub equip_slot_id: Option<u8>,
    pub equip_id: Option<u8>,
    pub members: bool,
    pub ge_tradeable: bool,
    pub buy_limit: Option<i32>,
    pub category: Option<u16>,
    pub dummy_item: Option<u8>,
    pub dummy_flag: bool,
    pub note_data: Option<u32>,
    pub note_template: Option<u32>,
    pub loan_id: Option<u32>,
    pub loan_template: Option<u32>,
    pub bind_link: Option<u32>,
    pub bind_template: Option<u32>,
    pub combine_info: Option<u32>,
    pub combine_template: Option<u32>,
    pub ground_actions: [Option<String>; 5],
    pub widget_actions: [Option<String>; 5],
    pub params: BTreeMap<u32, ParamValue>,
}

impl ItemDef {
    pub fn decode(id: u32, buf: &[u8]) -> Result<Self> {
        let mut c = Cursor::new(buf);
        let mut item = Self {
            id,
            ..Default::default()
        };

        loop {
            if c.remaining() == 0 {
                break;
            }
            let op = c.read_u8()?;
            match op {
                0 => break,
                1 => {
                    let _ = c.read_big_smart_opt()?;
                }
                2 => item.name = Some(c.read_jagex_string()?),
                3 => item.buff_effect = Some(c.read_jagex_string()?),
                4 | 5 | 6 | 7 | 8 | 10 | 18 | 44 | 45 | 95 | 110 | 111 | 112 | 142 | 143 | 144
                | 145 | 146 | 150 | 151 | 152 | 153 | 154 | 163 => {
                    let _ = c.read_u16()?;
                }
                9 => {
                    let len = c.read_u8()?;
                    for _ in 0..len {
                        let _ = c.read_big_smart_opt()?;
                    }
                }
                11 => item.stackable = true,
                12 => item.value = Some(c.read_i32()? as i64),
                13 => item.equip_slot_id = Some(c.read_u8()?),
                14 => item.equip_id = Some(c.read_u8()?),
                15 => {}
                16 => item.members = true,
                23 | 24 | 25 | 26 | 78 | 79 | 90 | 91 | 92 | 93 => {
                    let _ = c.read_big_smart_opt()?;
                }
                27 | 113 | 114 | 115 | 134 => {
                    let _ = c.read_u8()?;
                }
                30..=34 => {
                    item.ground_actions[(op - 30) as usize] = Some(c.read_jagex_string()?);
                }
                35..=39 => {
                    item.widget_actions[(op - 35) as usize] = Some(c.read_jagex_string()?);
                }
                40 | 41 => {
                    let len = c.read_u8()?;
                    c.skip(len as usize * 4)?;
                }
                42 => {
                    let len = c.read_u8()?;
                    c.skip(len as usize * 2)?;
                }
                43 => {
                    let _ = c.read_i32()?;
                }
                65 => item.ge_tradeable = true,
                69 => item.buy_limit = Some(c.read_i32()?),
                94 => item.category = Some(c.read_u16()?),
                96 => item.dummy_item = Some(c.read_u8()?),
                97 => item.note_data = Some(c.read_u16()? as u32),
                98 => item.note_template = Some(c.read_u16()? as u32),
                100..=109 => {
                    c.skip(4)?;
                }
                121 => item.loan_id = Some(c.read_u16()? as u32),
                122 => item.loan_template = Some(c.read_u16()? as u32),
                125 | 126 => {
                    c.skip(3)?;
                }
                127 | 128 | 129 | 130 => {
                    c.skip(3)?;
                }
                132 => {
                    let len = c.read_u8()?;
                    c.skip(len as usize * 3)?;
                }
                139 => item.bind_link = Some(c.read_u16()? as u32),
                140 => item.bind_template = Some(c.read_u16()? as u32),
                156 => item.dummy_flag = true,
                157 | 167 | 168 | 178 => {}
                161 => item.combine_info = Some(c.read_u16()? as u32),
                162 => item.combine_template = Some(c.read_u16()? as u32),
                164 => {
                    let _ = c.read_jagex_string()?;
                }
                165 => item.never_stackable = true,
                181 => item.value = Some(c.read_i64()?),
                182 => {
                    let _ = c.read_u24()?;
                }
                190..=199 => {
                    c.skip(5)?;
                }
                201 => item.note_data = Some(c.read_u24()?),
                202 => item.note_template = Some(c.read_u24()?),
                203 => item.loan_id = Some(c.read_u24()?),
                204 => item.loan_template = Some(c.read_u24()?),
                205 => item.bind_link = Some(c.read_u24()?),
                206 => item.bind_template = Some(c.read_u24()?),
                207 => item.combine_info = Some(c.read_u24()?),
                208 => item.combine_template = Some(c.read_u24()?),
                249 => {
                    let len = c.read_u8()?;
                    for _ in 0..len {
                        let is_string = c.read_u8()? == 1;
                        let key = c.read_u24()?;
                        let value = if is_string {
                            ParamValue::String(c.read_jagex_string()?)
                        } else {
                            ParamValue::Int(c.read_i32()?)
                        };
                        item.params.insert(key, value);
                    }
                }
                other => {
                    bail!(
                        "Unknown RS3 item opcode {other} (0x{other:02X}) on item {id} at offset {}/{}",
                        c.pos() - 1,
                        buf.len()
                    );
                }
            }
        }

        if c.remaining() != 0 {
            bail!(
                "Trailing {} bytes after opcode 0 on item {id}",
                c.remaining()
            );
        }

        Ok(item)
    }
}
