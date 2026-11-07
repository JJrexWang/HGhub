//! TS → MP4 无损换封装（不重新编码、不引外部工具）。
//!
//! 输入是下载拼接出的 MPEG-TS 成品（H.264/H.265 + AAC），解析 PAT/PMT/PES 抽裸流，
//! 视频做 Annex B → AVCC/HVCC 转换、AAC 剥 ADTS 帧头，再写标准 MP4
//! （ftyp + mdat + moov，样本按轨道连续存放，64 位偏移）。
//! 任一步遇到不支持的编码/结构就返回 Err，调用方保留原始 TS，不产生半成品 MP4。

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

/* ───────────────────────── 基础工具 ───────────────────────── */

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // bit 位置
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn bit(&mut self) -> Result<u32, String> {
        if self.pos / 8 >= self.data.len() {
            return Err("SPS 解析越界".into());
        }
        let b = (self.data[self.pos / 8] >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Ok(b as u32)
    }
    fn bits(&mut self, n: usize) -> Result<u32, String> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }
    fn ue(&mut self) -> Result<u32, String> {
        let mut zeros = 0usize;
        loop {
            if self.bit()? == 1 {
                break;
            }
            zeros += 1;
            if zeros > 31 {
                return Err(" Exp-Golomb 过长".into());
            }
        }
        if zeros == 0 {
            Ok(0)
        } else {
            Ok((1u32 << zeros) - 1 + self.bits(zeros)?)
        }
    }
    fn se(&mut self) -> Result<i32, String> {
        let v = self.ue()? as i32;
        Ok(if v % 2 == 0 { -(v / 2) } else { (v + 1) / 2 })
    }
}

/// 去掉防竞争字节（00 00 03 → 00 00）。
fn strip_epb(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

/* ───────────────────────── 样本与配置 ───────────────────────── */

#[derive(Clone, Default)]
struct VSample {
    off: u64, // 在轨道临时文件中的偏移
    size: u32,
    dts: i64, // 90k，已解卷绕
    pts: i64,
    key: bool,
}

#[derive(Clone, Default)]
struct ASample {
    off: u64,
    size: u32,
    pts90: i64,
}

#[derive(Default)]
struct AvcConfig {
    sps: Vec<u8>,
    pps: Vec<u8>,
    width: u32,
    height: u32,
}

#[derive(Default)]
struct HevcConfig {
    vps: Vec<u8>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    width: u32,
    height: u32,
    // hvcC 头需要从 SPS 里解析的字段
    profile_space: u32,
    tier: u32,
    profile_idc: u32,
    compat: u32,
    constraint: [u8; 6],
    level_idc: u32,
    chroma_idc: u32,
    bit_depth_luma_m8: u32,
    bit_depth_chroma_m8: u32,
    num_temporal: u32,
    temporal_nested: u32,
}

#[derive(Default)]
struct AacConfig {
    object_type: u32, // ADTS profile + 1
    sf_index: u32,
    sample_rate: u32,
    channels: u32,
}

enum VCodec {
    Avc(AvcConfig),
    Hevc(HevcConfig),
}

/* ───────────────────────── H.264 SPS 解析 ───────────────────────── */

fn skip_scaling_list(br: &mut BitReader, size: usize) -> Result<(), String> {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            let delta = br.se()?;
            next = (last + delta + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Ok(())
}

fn parse_avc_sps(nal: &[u8]) -> Result<(u32, u32), String> {
    // nal[0] 是 NAL 头，解析 RBSP
    let rbsp = strip_epb(&nal[1..]);
    let mut br = BitReader::new(&rbsp);
    let profile = br.bits(8)?;
    br.bits(8)?; // constraint flags
    br.bits(8)?; // level
    br.ue()?; // sps id
    let mut chroma = 1u32;
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma = br.ue()?;
        if chroma == 3 {
            br.bit()?; // separate_colour_plane
        }
        br.ue()?; // bit_depth_luma_minus8
        br.ue()?; // bit_depth_chroma_minus8
        br.bit()?; // qpprime_y_zero_transform_bypass
        if br.bit()? == 1 {
            // seq_scaling_matrix_present
            let n = if chroma != 3 { 8 } else { 12 };
            for i in 0..n {
                if br.bit()? == 1 {
                    skip_scaling_list(&mut br, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    br.ue()?; // log2_max_frame_num_minus4
    let po_type = br.ue()?;
    if po_type == 0 {
        br.ue()?;
    } else if po_type == 1 {
        br.bit()?;
        br.se()?;
        br.se()?;
        let cyc = br.ue()?;
        for _ in 0..cyc {
            br.se()?;
        }
    }
    br.ue()?; // max_num_ref_frames
    br.bit()?; // gaps
    let w_mbs = br.ue()? + 1;
    let h_map = br.ue()? + 1;
    let frame_mbs_only = br.bit()?;
    if frame_mbs_only == 0 {
        br.bit()?; // mb_adaptive_frame_field
    }
    br.bit()?; // direct_8x8
    let (mut cl, mut cr, mut ct, mut cb) = (0u32, 0u32, 0u32, 0u32);
    if br.bit()? == 1 {
        cl = br.ue()?;
        cr = br.ue()?;
        ct = br.ue()?;
        cb = br.ue()?;
    }
    // 裁剪单位：单色 1×(2-fmo)，4:2:0 为 2×2，4:2:2 为 2×1，4:4:4 为 1×1（再乘 2-fmo 得 y 单位）
    let (unit_x, unit_y) = match chroma {
        0 => (1, 2 - frame_mbs_only),
        1 => (2, 2 * (2 - frame_mbs_only)),
        2 => (2, 1 * (2 - frame_mbs_only)),
        _ => (1, 1 * (2 - frame_mbs_only)),
    };
    let width = w_mbs * 16 - (cl + cr) * unit_x;
    let height = (2 - frame_mbs_only) * h_map * 16 - (ct + cb) * unit_y;
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return Err("SPS 尺寸异常".into());
    }
    Ok((width, height))
}

/* ───────────────────────── H.265 SPS 解析 ───────────────────────── */

/// profile_tier_level 解析（general + 子层）。返回后 br 停在 level 之后。
fn parse_ptl(br: &mut BitReader, max_sub: u32, cfg: &mut HevcConfig, general: bool) -> Result<(), String> {
    if general {
        cfg.profile_space = br.bits(2)?;
        cfg.tier = br.bit()?;
        cfg.profile_idc = br.bits(5)?;
        cfg.compat = br.bits(32)?;
        for i in 0..6 {
            cfg.constraint[i] = br.bits(8)? as u8;
        }
        cfg.level_idc = br.bits(8)?;
    } else {
        br.bits(2)?;
        br.bit()?;
        br.bits(5)?;
        br.bits(32)?;
        for _ in 0..6 {
            br.bits(8)?;
        }
        br.bits(8)?;
    }
    if max_sub > 0 {
        let mut prof_present = Vec::new();
        let mut level_present = Vec::new();
        for _ in 0..max_sub {
            prof_present.push(br.bit()? == 1);
            level_present.push(br.bit()? == 1);
        }
        // reserved_zero_2bits × (8 - max_sub)
        for _ in 0..(8 - max_sub) {
            br.bits(2)?;
        }
        for i in 0..max_sub as usize {
            if prof_present[i] {
                br.bits(2)?;
                br.bit()?;
                br.bits(5)?;
                br.bits(32)?;
                for _ in 0..6 {
                    br.bits(8)?;
                }
            }
            if level_present[i] {
                br.bits(8)?;
            }
        }
    }
    Ok(())
}

fn parse_hevc_sps(nal: &[u8], cfg: &mut HevcConfig) -> Result<(), String> {
    // nal[0..2] 是 NAL 头
    if nal.len() < 3 {
        return Err("HEVC SPS 过短".into());
    }
    let rbsp = strip_epb(&nal[2..]);
    let mut br = BitReader::new(&rbsp);
    br.bits(4)?; // sps_video_parameter_set_id
    let max_sub = br.bits(3)?; // max_sub_layers_minus1
    cfg.num_temporal = max_sub + 1;
    cfg.temporal_nested = br.bit()?;
    parse_ptl(&mut br, max_sub, cfg, true)?;
    br.ue()?; // sps_seq_parameter_set_id
    let chroma = br.ue()?;
    cfg.chroma_idc = chroma;
    if chroma == 3 {
        br.bit()?; // separate_colour_plane
    }
    let w = br.ue()?;
    let h = br.ue()?;
    let (mut cl, mut cr, mut ct, mut cb) = (0u32, 0u32, 0u32, 0u32);
    if br.bit()? == 1 {
        // conformance_window
        cl = br.ue()?;
        cr = br.ue()?;
        ct = br.ue()?;
        cb = br.ue()?;
    }
    let (sub_w, sub_h) = match chroma {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    cfg.width = w.saturating_sub((cl + cr) * sub_w);
    cfg.height = h.saturating_sub((ct + cb) * sub_h);
    cfg.bit_depth_luma_m8 = br.ue()?;
    cfg.bit_depth_chroma_m8 = br.ue()?;
    if cfg.width == 0 || cfg.height == 0 {
        return Err("HEVC SPS 尺寸异常".into());
    }
    Ok(())
}

/* ───────────────────────── TS / PES 解析 ───────────────────────── */

const STREAM_H264: u8 = 0x1B;
const STREAM_H265: u8 = 0x24;
const STREAM_AAC: u8 = 0x0F;
const STREAM_AAC_LATM: u8 = 0x11;

struct Pes {
    pid: u16,
    pts: Option<i64>, // 已解卷绕（90k）
    dts: Option<i64>,
    data: Vec<u8>,
}

struct TsDemux {
    pmt_pid: Option<u16>,
    vpid: Option<u16>,
    apid: Option<u16>,
    vtype: u8,
    atype: u8,
    pes_buf: HashMap<u16, PesAcc>,
    last_ts: HashMap<u16, i64>, // 各 PID 上次的原始 33 位时间戳（解卷绕用）
    wrap_off: HashMap<u16, i64>,
    done: Vec<Pes>,
    unknown_audio: bool,
}

use std::collections::HashMap;

#[derive(Default)]
struct PesAcc {
    pid: u16,
    pts_raw: Option<i64>,
    dts_raw: Option<i64>,
    data: Vec<u8>,
    active: bool,
}

fn decode_ts33(b: &[u8]) -> i64 {
    // 5 字节时间戳编码 → 33 位值
    let v = (((b[0] >> 1) & 0x07) as i64) << 30
        | (b[1] as i64) << 22
        | (((b[2] >> 1) & 0x7F) as i64) << 15
        | (b[3] as i64) << 7
        | (((b[4] >> 1) & 0x7F) as i64);
    v
}

impl TsDemux {
    fn new() -> Self {
        Self {
            pmt_pid: None,
            vpid: None,
            apid: None,
            vtype: 0,
            atype: 0,
            pes_buf: HashMap::new(),
            last_ts: HashMap::new(),
            wrap_off: HashMap::new(),
            done: Vec::new(),
            unknown_audio: false,
        }
    }

    fn unwrap(&mut self, pid: u16, raw: i64) -> i64 {
        let off = *self.wrap_off.entry(pid).or_insert(0);
        if let Some(&last) = self.last_ts.get(&pid) {
            if raw + off < last - (1i64 << 32) {
                let off = off + (1i64 << 33);
                self.wrap_off.insert(pid, off);
            }
        }
        let v = raw + *self.wrap_off.get(&pid).unwrap_or(&0);
        self.last_ts.insert(pid, v);
        v
    }

    fn flush_pes(&mut self, pid: u16) {
        if let Some(acc) = self.pes_buf.get_mut(&pid) {
            if acc.active && !acc.data.is_empty() {
                let (pts, dts) = (acc.pts_raw, acc.dts_raw);
                let data = std::mem::take(&mut acc.data);
                acc.active = false;
                let (pts_u, dts_u) = (
                    pts.map(|r| self.unwrap(pid, r)),
                    dts.map(|r| self.unwrap(pid, r)),
                );
                self.done.push(Pes { pid, pts: pts_u, dts: dts_u, data });
            }
        }
    }

    fn feed_packet(&mut self, pkt: &[u8]) {
        if pkt.len() < 188 || pkt[0] != 0x47 {
            return;
        }
        let pusi = pkt[1] & 0x40 != 0;
        let pid = (((pkt[1] & 0x1F) as u16) << 8) | pkt[2] as u16;
        let afc = (pkt[3] >> 4) & 0x03;
        if afc == 0 || afc == 2 {
            return; // 无负载
        }
        let mut off = 4;
        if afc == 3 {
            let afl = pkt[4] as usize;
            off = 5 + afl;
            if off >= 188 {
                return;
            }
        }
        let payload = &pkt[off..188];

        if pid == 0 {
            // PAT
            if pusi {
                self.parse_pat(payload);
            }
            return;
        }
        if Some(pid) == self.pmt_pid {
            if pusi {
                self.parse_pmt(payload);
            }
            return;
        }
        if Some(pid) != self.vpid && Some(pid) != self.apid {
            return;
        }
        if pusi {
            self.flush_pes(pid);
            // PES 头解析
            if payload.len() < 9 || payload[0] != 0 || payload[1] != 0 || payload[2] != 1 {
                return;
            }
            let flags = payload[7];
            let hdr_len = payload[8] as usize;
            let pts_dts = (flags >> 6) & 0x03;
            let mut pts_raw = None;
            let mut dts_raw = None;
            if pts_dts == 0b10 || pts_dts == 0b11 || pts_dts == 0b01 {
                if payload.len() >= 14 {
                    pts_raw = Some(decode_ts33(&payload[9..14]));
                }
            }
            if pts_dts == 0b11 && payload.len() >= 19 {
                dts_raw = Some(decode_ts33(&payload[14..19]));
            }
            if pts_dts == 0b10 {
                dts_raw = pts_raw;
            }
            let data_off = 9 + hdr_len;
            if data_off > payload.len() {
                return;
            }
            let acc = self.pes_buf.entry(pid).or_default();
            acc.pid = pid;
            acc.pts_raw = pts_raw;
            acc.dts_raw = dts_raw;
            acc.data.clear();
            acc.data.extend_from_slice(&payload[data_off..]);
            acc.active = true;
        } else if let Some(acc) = self.pes_buf.get_mut(&pid) {
            if acc.active {
                acc.data.extend_from_slice(payload);
            }
        }
    }

    fn parse_pat(&mut self, payload: &[u8]) {
        let pointer = if payload.is_empty() { 0 } else { payload[0] as usize };
        let t = match payload.get(1 + pointer..) {
            Some(t) if t.len() >= 12 && t[0] == 0 => t,
            _ => return,
        };
        let sec_len = (((t[1] & 0x0F) as usize) << 8) | t[2] as usize;
        let end = (3 + sec_len).saturating_sub(4).min(t.len());
        let mut i = 8;
        while i + 4 <= end {
            let prog = ((t[i] as u16) << 8) | t[i + 1] as u16;
            let pid = (((t[i + 2] & 0x1F) as u16) << 8) | t[i + 3] as u16;
            if prog != 0 {
                self.pmt_pid = Some(pid);
                return;
            }
            i += 4;
        }
    }

    fn parse_pmt(&mut self, payload: &[u8]) {
        if self.vpid.is_some() {
            return; // 只解析第一版 PMT
        }
        let pointer = if payload.is_empty() { 0 } else { payload[0] as usize };
        let t = match payload.get(1 + pointer..) {
            Some(t) if t.len() >= 12 && t[0] == 2 => t,
            _ => return,
        };
        let sec_len = (((t[1] & 0x0F) as usize) << 8) | t[2] as usize;
        let prog_info_len = (((t[10] & 0x0F) as usize) << 8) | t[11] as usize;
        let mut i = 12 + prog_info_len;
        let end = (3 + sec_len).saturating_sub(4).min(t.len());
        while i + 5 <= end {
            let st = t[i];
            let pid = (((t[i + 1] & 0x1F) as u16) << 8) | t[i + 2] as u16;
            let es_info_len = (((t[i + 3] & 0x0F) as usize) << 8) | t[i + 4] as usize;
            match st {
                STREAM_H264 | STREAM_H265 => {
                    if self.vpid.is_none() {
                        self.vpid = Some(pid);
                        self.vtype = st;
                    }
                }
                STREAM_AAC | STREAM_AAC_LATM => {
                    if self.apid.is_none() {
                        self.apid = Some(pid);
                        self.atype = st;
                    }
                }
                0x03 | 0x04 | 0x81 | 0x84 | 0x87 => {
                    // MP1/MP2/AC-3 类音频：换封装不支持，标记后整体失败回退 TS
                    self.unknown_audio = true;
                }
                _ => {}
            }
            i += 5 + es_info_len;
        }
    }

    fn feed(&mut self, data: &[u8]) {
        for pkt in data.chunks_exact(188) {
            self.feed_packet(pkt);
        }
    }

    fn finish(&mut self) -> Vec<Pes> {
        let pids: Vec<u16> = self.pes_buf.keys().cloned().collect();
        for pid in pids {
            self.flush_pes(pid);
        }
        std::mem::take(&mut self.done)
    }
}

/* ───────────────────────── 裸流抽取 ───────────────────────── */

/// Annex B 切 NAL：返回 (nal_type, nal_bytes含头) 列表。
fn split_nals(data: &[u8], hevc: bool) -> Vec<(u32, &[u8])> {
    let mut out = Vec::new();
    let mut starts: Vec<(usize, usize)> = Vec::new(); // (数据起点, 起始码长度)
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && (data[i + 2] == 1 || (i + 4 < data.len() && data[i + 2] == 0 && data[i + 3] == 1)) {
            let sc = if data[i + 2] == 1 { 3 } else { 4 };
            starts.push((i + sc, sc));
            i += sc;
        } else {
            i += 1;
        }
    }
    for (idx, &(start, _)) in starts.iter().enumerate() {
        let end = if idx + 1 < starts.len() {
            starts[idx + 1].0 - starts[idx + 1].1
        } else {
            data.len()
        };
        if start >= end {
            continue;
        }
        let nal = &data[start..end];
        let t = if hevc {
            if nal.len() < 2 {
                continue;
            }
            ((nal[0] >> 1) & 0x3F) as u32
        } else {
            if nal.is_empty() {
                continue;
            }
            (nal[0] & 0x1F) as u32
        };
        out.push((t, nal));
    }
    out
}

struct ExtractOut {
    codec: VCodec,
    vsamples: Vec<VSample>,
    asamples: Vec<ASample>,
    acfg: Option<AacConfig>,
    atype: u8,
    vtmp_len: u64,
    atmp_len: u64,
}

fn extract(
    pes_list: Vec<Pes>,
    demux: &TsDemux,
    vtmp: &mut File,
    atmp: &mut File,
) -> Result<ExtractOut, String> {
    let hevc = demux.vtype == STREAM_H265;
    let mut avc = AvcConfig::default();
    let mut hvc = HevcConfig::default();
    let mut have_avc = false;
    let mut have_hvc = false;

    let mut vsamples: Vec<VSample> = Vec::new();
    let mut cur: Vec<u8> = Vec::new(); // 当前 AU（AVCC 格式）
    let mut cur_has_vcl = false;
    let mut cur_key = false;
    let mut cur_pts: Option<i64> = None;
    let mut cur_dts: Option<i64> = None;
    let mut prefix: Vec<u8> = Vec::new(); // 只有 SEI 等的 PES 先攒着，挂到下一 AU
    let mut voff = 0u64;

    fn push_sample(
        vsamples: &mut Vec<VSample>,
        cur: &mut Vec<u8>,
        vtmp: &mut File,
        voff: &mut u64,
        pts: Option<i64>,
        dts: Option<i64>,
        key: bool,
    ) -> Result<(), String> {
        if cur.is_empty() {
            return Ok(());
        }
        vtmp.write_all(cur).map_err(|e| format!("写视频样本失败：{e}"))?;
        vsamples.push(VSample {
            off: *voff,
            size: cur.len() as u32,
            dts: dts.unwrap_or(i64::MIN / 2),
            pts: pts.unwrap_or(i64::MIN / 2),
            key,
        });
        *voff += cur.len() as u64;
        cur.clear();
        Ok(())
    }

    for pes in pes_list.iter().filter(|p| Some(p.pid) == demux.vpid) {
        let nals = split_nals(&pes.data, hevc);
        for (t, nal) in nals {
            let (is_vcl, is_key, is_aud, is_sps, is_pps, is_vps) = if hevc {
                (t <= 31, t == 19 || t == 20, t == 35, t == 33, t == 34, t == 32)
            } else {
                ((1..=5).contains(&t), t == 5, t == 9, t == 7, t == 8, false)
            };
            if is_vps {
                if hvc.vps.is_empty() {
                    hvc.vps = nal.to_vec();
                }
                continue;
            }
            if is_sps {
                if hevc {
                    if hvc.sps.is_empty() {
                        hvc.sps = nal.to_vec();
                        parse_hevc_sps(nal, &mut hvc)?;
                        have_hvc = true;
                    }
                } else if avc.sps.is_empty() {
                    avc.sps = nal.to_vec();
                    let (w, h) = parse_avc_sps(nal)?;
                    avc.width = w;
                    avc.height = h;
                    have_avc = true;
                }
                continue;
            }
            if is_pps {
                if hevc {
                    if hvc.pps.is_empty() {
                        hvc.pps = nal.to_vec();
                    }
                } else if avc.pps.is_empty() {
                    avc.pps = nal.to_vec();
                }
                continue;
            }
            if is_aud {
                if cur_has_vcl {
                    push_sample(&mut vsamples, &mut cur, vtmp, &mut voff, cur_pts, cur_dts, cur_key)?;
                    cur_has_vcl = false;
                    cur_key = false;
                    cur_pts = None;
                    cur_dts = None;
                }
                continue;
            }
            if is_vcl {
                // 新 AU 判定：当前 AU 已有 VCL 且本 NAL 是本片首 MB/首段
                let rbsp0 = if hevc {
                    nal.get(2).copied().unwrap_or(0)
                } else {
                    nal.get(1).copied().unwrap_or(0)
                };
                if cur_has_vcl && (rbsp0 & 0x80) != 0 {
                    push_sample(&mut vsamples, &mut cur, vtmp, &mut voff, cur_pts, cur_dts, cur_key)?;
                    cur_has_vcl = false;
                    cur_key = false;
                    cur_pts = None;
                    cur_dts = None;
                }
                if !cur_has_vcl && cur.is_empty() && !prefix.is_empty() {
                    cur.extend_from_slice(&prefix);
                    prefix.clear();
                }
                if cur_pts.is_none() {
                    cur_pts = pes.pts;
                    cur_dts = pes.dts.or(pes.pts);
                }
                cur_has_vcl = true;
                if is_key {
                    cur_key = true;
                }
            }
            // 写入 AVCC 格式（4 字节长度 + NAL）
            cur.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            cur.extend_from_slice(nal);
        }
        // PES 里只有 SEI/其它且还没攒出 VCL：整段留作前缀
        if !cur_has_vcl && !cur.is_empty() {
            prefix.extend_from_slice(&cur);
            cur.clear();
            cur_pts = None;
            cur_dts = None;
        }
    }
    if cur_has_vcl {
        push_sample(&mut vsamples, &mut cur, vtmp, &mut voff, cur_pts, cur_dts, cur_key)?;
    }
    if vsamples.is_empty() {
        return Err("未抽到视频样本".into());
    }
    let codec = if hevc {
        if !have_hvc || hvc.vps.is_empty() || hvc.pps.is_empty() {
            return Err("HEVC 缺少 VPS/SPS/PPS".into());
        }
        VCodec::Hevc(hvc)
    } else {
        if !have_avc || avc.pps.is_empty() {
            return Err("H.264 缺少 SPS/PPS".into());
        }
        VCodec::Avc(avc)
    };

    // ── 音频：ADTS 逐帧剥头 ──
    let mut asamples: Vec<ASample> = Vec::new();
    let mut acfg: Option<AacConfig> = None;
    let mut aoff = 0u64;
    let mut last_pts90: Option<i64> = None;
    if demux.apid.is_some() {
        if demux.atype == STREAM_AAC_LATM {
            return Err("LATM 打包的 AAC 暂不支持换封装".into());
        }
        for pes in pes_list.iter().filter(|p| Some(p.pid) == demux.apid) {
            let mut i = 0usize;
            let mut frame_idx: i64 = 0;
            while i + 7 <= pes.data.len() {
                let d = &pes.data[i..];
                if d[0] != 0xFF || (d[1] & 0xF0) != 0xF0 {
                    break;
                }
                let protection_absent = d[1] & 0x01;
                let profile = ((d[2] >> 6) & 0x03) as u32;
                let sf_index = ((d[2] >> 2) & 0x0F) as u32;
                let channels = (((d[2] & 0x01) as u32) << 2) | ((d[3] >> 6) & 0x03) as u32;
                let frame_len = (((d[3] & 0x03) as usize) << 11) | ((d[4] as usize) << 3) | ((d[5] >> 5) & 0x07) as usize;
                if frame_len < 7 || i + frame_len > pes.data.len() {
                    break;
                }
                let sr = *SAMPLE_RATES.get(sf_index as usize).unwrap_or(&0);
                if sr == 0 {
                    return Err("AAC 采样率异常".into());
                }
                if acfg.is_none() {
                    acfg = Some(AacConfig {
                        object_type: profile + 1,
                        sf_index,
                        sample_rate: sr,
                        channels,
                    });
                }
                let hdr = if protection_absent == 1 { 7 } else { 9 };
                let raw = &d[hdr..frame_len];
                let pts90 = match pes.pts {
                    Some(p) => p + frame_idx * 1024 * 90000 / sr as i64,
                    None => match last_pts90 {
                        Some(p) => p + 1024 * 90000 / sr as i64,
                        None => 0,
                    },
                };
                last_pts90 = Some(pts90);
                atmp.write_all(raw).map_err(|e| format!("写音频样本失败：{e}"))?;
                asamples.push(ASample { off: aoff, size: raw.len() as u32, pts90 });
                aoff += raw.len() as u64;
                i += frame_len;
                frame_idx += 1;
            }
        }
    }

    Ok(ExtractOut {
        codec,
        vsamples,
        asamples,
        acfg,
        atype: demux.atype,
        vtmp_len: voff,
        atmp_len: aoff,
    })
}

const SAMPLE_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/* ───────────────────────── MP4 写出 ───────────────────────── */

fn bx(typ: &str, mut payload: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(typ.as_bytes());
    out.append(&mut payload);
    out
}

fn full_bx(typ: &str, version: u8, flags: u32, mut body: Vec<u8>) -> Vec<u8> {
    let mut p = Vec::with_capacity(body.len() + 4);
    p.push(version);
    p.extend_from_slice(&(flags & 0x00FF_FFFF).to_be_bytes()[1..]);
    p.append(&mut body);
    bx(typ, p)
}

fn u32b(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_be_bytes());
}
fn u64b(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_be_bytes());
}
fn u16b(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_be_bytes());
}

/// 补齐/修复视频样本时间戳（90k）：缺失插值、强制 DTS 单调递增。
/// 返回 (durations, ctts, base)：base 是归零前的首个 DTS（90k），供音视频对齐。
fn fix_video_times(samples: &mut [VSample]) -> (Vec<u32>, Vec<u32>, i64) {
    let n = samples.len();
    let missing = |v: i64| v < -(1i64 << 40);
    // 锚点间线性插值
    let anchors: Vec<usize> = (0..n).filter(|&i| !missing(samples[i].dts)).collect();
    if !anchors.is_empty() {
        for w in anchors.windows(2) {
            let (a, b) = (w[0], w[1]);
            let (da, db) = (samples[a].dts, samples[b].dts);
            for (k, j) in ((a + 1)..b).enumerate() {
                let frac = (k + 1) as i64;
                let span = (b - a) as i64;
                samples[j].dts = da + (db - da) * frac / span.max(1);
            }
        }
        let first = anchors[0];
        let est = if anchors.len() > 1 {
            let span = samples[anchors[1]].dts - samples[first].dts;
            (span / (anchors[1] - first).max(1) as i64).max(1)
        } else {
            3000
        };
        for j in (0..first).rev() {
            samples[j].dts = samples[first].dts - (first - j) as i64 * est;
        }
        let last = *anchors.last().unwrap();
        for j in (last + 1)..n {
            samples[j].dts = samples[last].dts + (j - last) as i64 * est;
        }
    } else {
        for (i, s) in samples.iter_mut().enumerate() {
            s.dts = i as i64 * 3000;
        }
    }
    for i in 1..n {
        if samples[i].dts <= samples[i - 1].dts {
            samples[i].dts = samples[i - 1].dts + 1;
        }
    }
    // PTS：缺失则用上一帧的合成偏移补
    let mut last_off = 0i64;
    for i in 0..n {
        if missing(samples[i].pts) {
            samples[i].pts = samples[i].dts + last_off;
        } else {
            last_off = samples[i].pts - samples[i].dts;
        }
        if samples[i].pts < samples[i].dts {
            samples[i].pts = samples[i].dts;
        }
    }
    let base = samples[0].dts;
    for s in samples.iter_mut() {
        s.dts -= base;
        s.pts -= base;
    }
    let mut durations = Vec::with_capacity(n);
    for i in 0..n {
        let d = if i + 1 < n {
            (samples[i + 1].dts - samples[i].dts).max(1) as u32
        } else if i > 0 {
            durations[i - 1]
        } else {
            3000
        };
        durations.push(d);
    }
    // B 帧重排使末帧呈现结束晚于 DTS 跨度：把缺口补到最后一帧时长上，
    // 否则轨道时长不够，播放器会丢掉末尾的帧（ffmpeg movenc 同样处理）。
    let present_end = samples
        .iter()
        .zip(durations.iter())
        .map(|(s, &d)| s.pts + d as i64)
        .max()
        .unwrap_or(0);
    let decode_end = samples[n - 1].dts + durations[n - 1] as i64;
    if present_end > decode_end {
        durations[n - 1] += (present_end - decode_end) as u32;
    }
    let ctts: Vec<u32> = samples.iter().map(|s| (s.pts - s.dts).max(0) as u32).collect();
    (durations, ctts, base)
}

fn rle_durations(durations: &[u32]) -> Vec<u8> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &d in durations {
        if let Some(last) = runs.last_mut() {
            if last.1 == d {
                last.0 += 1;
                continue;
            }
        }
        runs.push((1, d));
    }
    let mut v = Vec::new();
    u32b(&mut v, runs.len() as u32);
    for (count, delta) in runs {
        u32b(&mut v, count);
        u32b(&mut v, delta);
    }
    v
}

fn elst_box(offset_ms: u32, media_duration_ms: u32) -> Vec<u8> {
    let mut body = Vec::new();
    if offset_ms > 0 {
        u32b(&mut body, 2);
        u32b(&mut body, offset_ms);
        u32b(&mut body, (-1i32) as u32);
        u32b(&mut body, media_duration_ms);
        u32b(&mut body, 0);
    } else {
        u32b(&mut body, 1);
        u32b(&mut body, media_duration_ms);
        u32b(&mut body, 0);
    }
    u16b(&mut body, 1); // media_rate_integer
    u16b(&mut body, 0);
    full_bx("elst", 0, 0, body)
}

fn mvhd_box(duration_ms: u32) -> Vec<u8> {
    let mut b = Vec::new();
    u32b(&mut b, 0); // creation
    u32b(&mut b, 0); // modification
    u32b(&mut b, 1000); // timescale
    u32b(&mut b, duration_ms);
    u32b(&mut b, 0x0001_0000); // rate 1.0
    u16b(&mut b, 0x0100); // volume
    u16b(&mut b, 0);
    u32b(&mut b, 0);
    u32b(&mut b, 0);
    for m in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        u32b(&mut b, m);
    }
    for _ in 0..6 {
        u32b(&mut b, 0);
    }
    u32b(&mut b, 3); // next_track_id
    full_bx("mvhd", 0, 0, b)
}

fn tkhd_box(track_id: u32, duration_ms: u32, width: u32, height: u32, audio: bool) -> Vec<u8> {
    let mut b = Vec::new();
    u32b(&mut b, 0);
    u32b(&mut b, 0);
    u32b(&mut b, track_id);
    u32b(&mut b, 0); // reserved
    u32b(&mut b, duration_ms);
    u32b(&mut b, 0);
    u32b(&mut b, 0);
    b.extend_from_slice(&0i16.to_be_bytes()); // layer
    b.extend_from_slice(&0i16.to_be_bytes()); // alternate_group
    u16b(&mut b, if audio { 0x0100 } else { 0 }); // volume
    u16b(&mut b, 0);
    for m in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        u32b(&mut b, m);
    }
    u32b(&mut b, width << 16);
    u32b(&mut b, height << 16);
    full_bx("tkhd", 0, 0x07, b)
}

fn mdhd_box(timescale: u32, duration: u64) -> Vec<u8> {
    let mut b = Vec::new();
    u32b(&mut b, 0);
    u32b(&mut b, 0);
    u32b(&mut b, timescale);
    u32b(&mut b, duration as u32);
    u16b(&mut b, 0x55C4); // 'und'
    u16b(&mut b, 0);
    full_bx("mdhd", 0, 0, b)
}

fn hdlr_box(kind: &str, name: &str) -> Vec<u8> {
    let mut b = Vec::new();
    u32b(&mut b, 0); // pre_defined
    b.extend_from_slice(kind.as_bytes());
    for _ in 0..3 {
        u32b(&mut b, 0);
    }
    b.extend_from_slice(name.as_bytes());
    b.push(0);
    full_bx("hdlr", 0, 0, b)
}

fn dinf_box() -> Vec<u8> {
    let url = full_bx("url ", 0, 1, Vec::new());
    let mut dref_body = Vec::new();
    u32b(&mut dref_body, 1);
    dref_body.extend_from_slice(&url);
    let dref = full_bx("dref", 0, 0, dref_body);
    bx("dinf", dref)
}

fn avcc_box(cfg: &AvcConfig) -> Vec<u8> {
    let mut p = Vec::new();
    p.push(1);
    p.push(cfg.sps.get(1).copied().unwrap_or(0x64));
    p.push(cfg.sps.get(2).copied().unwrap_or(0));
    p.push(cfg.sps.get(3).copied().unwrap_or(0x28));
    p.push(0xFF);
    p.push(0xE1);
    u16b(&mut p, cfg.sps.len() as u16);
    p.extend_from_slice(&cfg.sps);
    p.push(1);
    u16b(&mut p, cfg.pps.len() as u16);
    p.extend_from_slice(&cfg.pps);
    bx("avcC", p)
}

fn hvcc_box(cfg: &HevcConfig) -> Vec<u8> {
    let mut p = Vec::new();
    p.push(1);
    p.push(((cfg.profile_space << 6) | (cfg.tier << 5) | cfg.profile_idc) as u8);
    u32b(&mut p, cfg.compat);
    p.extend_from_slice(&cfg.constraint);
    p.push(cfg.level_idc as u8);
    u16b(&mut p, 0xF000); // min_spatial_segmentation_idc = 0
    p.push(0xFC); // parallelismType = 0
    p.push(0xFC | (cfg.chroma_idc & 3) as u8);
    p.push(0xF8 | (cfg.bit_depth_luma_m8 & 7) as u8);
    p.push(0xF8 | (cfg.bit_depth_chroma_m8 & 7) as u8);
    u16b(&mut p, 0); // avg_frame_rate
    p.push((((cfg.num_temporal & 7) << 3) | ((cfg.temporal_nested & 1) << 2) | 3) as u8);
    p.push(3); // numOfArrays
    for (t, nal) in [(32u8, &cfg.vps), (33, &cfg.sps), (34, &cfg.pps)] {
        p.push(0x80 | t);
        u16b(&mut p, 1);
        u16b(&mut p, nal.len() as u16);
        p.extend_from_slice(nal);
    }
    bx("hvcC", p)
}

fn esds_box(cfg: &AacConfig, avg_bitrate: u32) -> Vec<u8> {
    let asc = ((cfg.object_type & 0x1F) << 11) | ((cfg.sf_index & 0x0F) << 7) | ((cfg.channels & 0x0F) << 3);
    let mut dsi = Vec::new();
    dsi.push(0x05);
    dsi.push(2);
    u16b(&mut dsi, asc as u16);
    let mut dc = Vec::new();
    dc.push(0x04);
    dc.push((13 + dsi.len()) as u8);
    dc.push(0x40); // objectTypeIndication: AAC
    dc.push(0x15); // streamType: audio
    dc.extend_from_slice(&[0, 0, 0]); // bufferSizeDB
    u32b(&mut dc, avg_bitrate);
    u32b(&mut dc, avg_bitrate);
    dc.extend_from_slice(&dsi);
    let mut sl = Vec::new();
    sl.push(0x06);
    sl.push(1);
    sl.push(0x02);
    let mut body = Vec::new();
    body.push(0x03);
    body.push((3 + dc.len() + sl.len()) as u8);
    u16b(&mut body, 0); // ES_ID
    body.push(0); // flags
    body.extend_from_slice(&dc);
    body.extend_from_slice(&sl);
    full_bx("esds", 0, 0, body)
}

fn video_stsd(codec: &VCodec) -> Vec<u8> {
    let (w, h, cfg_bx, name) = match codec {
        VCodec::Avc(c) => (c.width, c.height, avcc_box(c), "avc1"),
        VCodec::Hevc(c) => (c.width, c.height, hvcc_box(c), "hvc1"),
    };
    let mut e = Vec::new();
    e.extend_from_slice(&[0u8; 6]);
    u16b(&mut e, 1);
    u16b(&mut e, 0);
    u16b(&mut e, 0);
    u32b(&mut e, 0);
    u32b(&mut e, 0);
    u32b(&mut e, 0);
    u16b(&mut e, w as u16);
    u16b(&mut e, h as u16);
    u32b(&mut e, 0x0048_0000);
    u32b(&mut e, 0x0048_0000);
    u32b(&mut e, 0);
    u16b(&mut e, 1);
    let mut cname = [0u8; 32];
    cname[0] = 5;
    cname[1..6].copy_from_slice(b"HGhub");
    e.extend_from_slice(&cname);
    u16b(&mut e, 0x0018);
    e.extend_from_slice(&(-1i16).to_be_bytes());
    e.extend_from_slice(&cfg_bx);
    let mut body = Vec::new();
    u32b(&mut body, 1);
    body.extend_from_slice(&bx(name, e));
    full_bx("stsd", 0, 0, body)
}

fn audio_stsd(cfg: &AacConfig, avg_bitrate: u32) -> Vec<u8> {
    let mut e = Vec::new();
    e.extend_from_slice(&[0u8; 6]);
    u16b(&mut e, 1);
    e.extend_from_slice(&[0u8; 8]);
    u16b(&mut e, cfg.channels as u16);
    u16b(&mut e, 16);
    u16b(&mut e, 0);
    u16b(&mut e, 0);
    u32b(&mut e, cfg.sample_rate << 16);
    e.extend_from_slice(&esds_box(cfg, avg_bitrate));
    let mut body = Vec::new();
    u32b(&mut body, 1);
    body.extend_from_slice(&bx("mp4a", e));
    full_bx("stsd", 0, 0, body)
}



fn sc_sz_co(sizes: &[u32], offsets: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut sc = Vec::new();
    u32b(&mut sc, 1);
    u32b(&mut sc, 1);
    u32b(&mut sc, 1);
    u32b(&mut sc, 1);
    out.extend_from_slice(&full_bx("stsc", 0, 0, sc));
    let mut sz = Vec::new();
    u32b(&mut sz, 0);
    u32b(&mut sz, sizes.len() as u32);
    for s in sizes {
        u32b(&mut sz, *s);
    }
    out.extend_from_slice(&full_bx("stsz", 0, 0, sz));
    let mut co = Vec::new();
    u32b(&mut co, offsets.len() as u32);
    for o in offsets {
        u64b(&mut co, *o);
    }
    out.extend_from_slice(&full_bx("co64", 0, 0, co));
    out
}

fn video_trak(
    codec: &VCodec,
    samples: &[VSample],
    durations: &[u32],
    ctts: &[u32],
    file_offsets: &[u64],
    offset_ms: u32,
) -> Vec<u8> {
    let dur_units: u64 = durations.iter().map(|&d| d as u64).sum();
    let dur_ms = (dur_units * 1000 / 90000) as u32;
    let mut stbl = Vec::new();
    stbl.extend_from_slice(&video_stsd(codec));
    stbl.extend_from_slice(&full_bx("stts", 0, 0, rle_durations(durations)));
    if ctts.iter().any(|&c| c > 0) {
        stbl.extend_from_slice(&full_bx("ctts", 0, 0, rle_durations(ctts)));
    }
    let keys: Vec<u32> = samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.key)
        .map(|(i, _)| i as u32 + 1)
        .collect();
    if !keys.is_empty() && keys.len() < samples.len() {
        let mut ss = Vec::new();
        u32b(&mut ss, keys.len() as u32);
        for k in &keys {
            u32b(&mut ss, *k);
        }
        stbl.extend_from_slice(&full_bx("stss", 0, 0, ss));
    }
    let sizes: Vec<u32> = samples.iter().map(|s| s.size).collect();
    stbl.extend_from_slice(&sc_sz_co(&sizes, file_offsets));

    let mut vmhd_body = Vec::new();
    u16b(&mut vmhd_body, 0);
    u16b(&mut vmhd_body, 0);
    u16b(&mut vmhd_body, 0);
    u16b(&mut vmhd_body, 0);
    let mut minf = Vec::new();
    minf.extend_from_slice(&full_bx("vmhd", 0, 1, vmhd_body));
    minf.extend_from_slice(&dinf_box());
    minf.extend_from_slice(&bx("stbl", stbl));

    let (w, h) = match codec {
        VCodec::Avc(c) => (c.width, c.height),
        VCodec::Hevc(c) => (c.width, c.height),
    };
    let mut mdia = Vec::new();
    mdia.extend_from_slice(&mdhd_box(90000, dur_units));
    mdia.extend_from_slice(&hdlr_box("vide", "VideoHandler"));
    mdia.extend_from_slice(&bx("minf", minf));

    let mut edts = Vec::new();
    edts.extend_from_slice(&elst_box(offset_ms, dur_ms));
    let mut trak = Vec::new();
    trak.extend_from_slice(&tkhd_box(1, dur_ms, w, h, false));
    trak.extend_from_slice(&bx("edts", edts));
    trak.extend_from_slice(&bx("mdia", mdia));
    bx("trak", trak)
}

fn audio_trak(
    cfg: &AacConfig,
    samples: &[ASample],
    durations: &[u32],
    file_offsets: &[u64],
    offset_ms: u32,
    total_bytes: u64,
) -> Vec<u8> {
    let dur_units: u64 = durations.iter().map(|&d| d as u64).sum();
    let dur_ms = if cfg.sample_rate > 0 {
        (dur_units * 1000 / cfg.sample_rate as u64) as u32
    } else {
        0
    };
    let avg_bitrate = if dur_units > 0 {
        (total_bytes * 8 * cfg.sample_rate as u64 / dur_units) as u32
    } else {
        0
    };
    let mut stbl = Vec::new();
    stbl.extend_from_slice(&audio_stsd(cfg, avg_bitrate));
    stbl.extend_from_slice(&full_bx("stts", 0, 0, rle_durations(durations)));
    let sizes: Vec<u32> = samples.iter().map(|s| s.size).collect();
    stbl.extend_from_slice(&sc_sz_co(&sizes, file_offsets));

    let mut smhd_body = Vec::new();
    u16b(&mut smhd_body, 0);
    u16b(&mut smhd_body, 0);
    let mut minf = Vec::new();
    minf.extend_from_slice(&full_bx("smhd", 0, 0, smhd_body));
    minf.extend_from_slice(&dinf_box());
    minf.extend_from_slice(&bx("stbl", stbl));

    let mut mdia = Vec::new();
    mdia.extend_from_slice(&mdhd_box(cfg.sample_rate, dur_units));
    mdia.extend_from_slice(&hdlr_box("soun", "SoundHandler"));
    mdia.extend_from_slice(&bx("minf", minf));

    let mut edts = Vec::new();
    edts.extend_from_slice(&elst_box(offset_ms, dur_ms));
    let mut trak = Vec::new();
    trak.extend_from_slice(&tkhd_box(2, dur_ms, 0, 0, true));
    trak.extend_from_slice(&bx("edts", edts));
    trak.extend_from_slice(&bx("mdia", mdia));
    bx("trak", trak)
}

/* ───────────────────────── 入口 ───────────────────────── */

pub struct RemuxInfo {
    pub codec: &'static str,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
    pub has_audio: bool,
}

pub fn remux_ts_to_mp4(input: &Path, output: &Path) -> Result<RemuxInfo, String> {
    // 1) 解复用（流式读，188 对齐结转）
    let mut demux = TsDemux::new();
    let mut f = File::open(input).map_err(|e| format!("打开源文件失败：{e}"))?;
    let mut carry: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 188 * 4096];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("读源文件失败：{e}"))?;
        if n == 0 {
            break;
        }
        carry.extend_from_slice(&buf[..n]);
        let usable = carry.len() / 188 * 188;
        if usable > 0 {
            demux.feed(&carry[..usable]);
            carry.drain(..usable);
        }
    }
    if demux.vpid.is_none() {
        return Err("未找到视频轨".into());
    }
    if demux.unknown_audio {
        return Err("音频编码不是 AAC，暂不支持换封装".into());
    }
    let pes_list = demux.finish();

    // 2) 抽裸流到临时文件
    let vtmp_path = output.with_extension("vtmp");
    let atmp_path = output.with_extension("atmp");
    let cleanup = |vp: &Path, ap: &Path| {
        let _ = std::fs::remove_file(vp);
        let _ = std::fs::remove_file(ap);
    };
    let mut vtmp = File::create(&vtmp_path).map_err(|e| format!("建临时文件失败：{e}"))?;
    let mut atmp = File::create(&atmp_path).map_err(|e| format!("建临时文件失败：{e}"))?;
    let ext = match extract(pes_list, &demux, &mut vtmp, &mut atmp) {
        Ok(x) => x,
        Err(e) => {
            cleanup(&vtmp_path, &atmp_path);
            return Err(e);
        }
    };
    drop(vtmp);
    drop(atmp);

    // 3) 时间戳整理（视频首 DTS 与音频首 PTS 的相对偏移 → elst 空编辑对齐）
    let mut vsamples = ext.vsamples;
    let (vdurations, vctts, v_base) = fix_video_times(&mut vsamples);
    let (sr, a_durations, a_off_ms, v_off_ms) = if !ext.asamples.is_empty() {
        let cfg = ext.acfg.as_ref().ok_or("缺少 AAC 配置")?;
        let a_start = ext.asamples[0].pts90;
        let base90 = v_base.min(a_start);
        let v_off = ((v_base - base90) * 1000 / 90000) as u32;
        let a_off = ((a_start - base90) * 1000 / 90000) as u32;
        let mut durs = Vec::with_capacity(ext.asamples.len());
        for i in 0..ext.asamples.len() {
            let d = if i + 1 < ext.asamples.len() {
                let delta90 = ext.asamples[i + 1].pts90 - ext.asamples[i].pts90;
                ((delta90.max(1) as i128 * cfg.sample_rate as i128) / 90000) as u32
            } else {
                1024
            };
            durs.push(d.max(1));
        }
        (cfg.sample_rate, durs, a_off, v_off)
    } else {
        (0, Vec::new(), 0, 0)
    };

    // 4) 组装 MP4
    let mut ftyp = Vec::new();
    ftyp.extend_from_slice(b"isom");
    ftyp.extend_from_slice(&0x200u32.to_be_bytes());
    ftyp.extend_from_slice(b"isomiso2");
    ftyp.extend_from_slice(match ext.codec {
        VCodec::Avc(_) => b"avc1",
        VCodec::Hevc(_) => b"hvc1",
    });
    ftyp.extend_from_slice(b"mp41");
    let ftyp = bx("ftyp", ftyp);
    let mdat_data_start = (ftyp.len() + 16) as u64;
    let v_offsets: Vec<u64> = vsamples.iter().map(|s| mdat_data_start + s.off).collect();
    let a_offsets: Vec<u64> = ext
        .asamples
        .iter()
        .map(|s| mdat_data_start + ext.vtmp_len + s.off)
        .collect();

    let v_dur_ms = (vdurations.iter().map(|&d| d as u64).sum::<u64>() * 1000 / 90000) as u32;
    let a_dur_ms = if sr > 0 {
        (a_durations.iter().map(|&d| d as u64).sum::<u64>() * 1000 / sr as u64) as u32
    } else {
        0
    };
    let movie_dur = (v_off_ms + v_dur_ms).max(a_off_ms + a_dur_ms);

    let mut moov = Vec::new();
    moov.extend_from_slice(&mvhd_box(movie_dur));
    moov.extend_from_slice(&video_trak(&ext.codec, &vsamples, &vdurations, &vctts, &v_offsets, v_off_ms));
    if !ext.asamples.is_empty() {
        let cfg = ext.acfg.as_ref().unwrap();
        moov.extend_from_slice(&audio_trak(cfg, &ext.asamples, &a_durations, &a_offsets, a_off_ms, ext.atmp_len));
    }
    let moov = bx("moov", moov);

    let mut out = File::create(output).map_err(|e| format!("建输出文件失败：{e}"))?;
    out.write_all(&ftyp).map_err(|e| e.to_string())?;
    let mdat_total = 16 + ext.vtmp_len + ext.atmp_len;
    out.write_all(&1u32.to_be_bytes()).map_err(|e| e.to_string())?;
    out.write_all(b"mdat").map_err(|e| e.to_string())?;
    out.write_all(&mdat_total.to_be_bytes()).map_err(|e| e.to_string())?;
    let mut vf = File::open(&vtmp_path).map_err(|e| e.to_string())?;
    std::io::copy(&mut vf, &mut out).map_err(|e| e.to_string())?;
    if ext.atmp_len > 0 {
        let mut af = File::open(&atmp_path).map_err(|e| e.to_string())?;
        std::io::copy(&mut af, &mut out).map_err(|e| e.to_string())?;
    }
    out.write_all(&moov).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;
    drop(out);
    cleanup(&vtmp_path, &atmp_path);

    let (w, h, cname) = match &ext.codec {
        VCodec::Avc(c) => (c.width, c.height, "h264"),
        VCodec::Hevc(c) => (c.width, c.height, "h265"),
    };
    Ok(RemuxInfo {
        codec: cname,
        width: w,
        height: h,
        duration_ms: movie_dur as u64,
        has_audio: !ext.asamples.is_empty(),
    })
}

/* ───────────────────────── 测试（ffmpeg 造流 + ffprobe 验收） ───────────────────────── */

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn run(cmd: &str, args: &[&str]) -> Option<String> {
        let out = Command::new(cmd).args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    }

    fn gen_ts(dir: &Path, name: &str, vcodec: &str, audio: bool) -> Option<PathBuf> {
        let path = dir.join(name);
        let mut args: Vec<&str> = vec!["-hide_banner", "-loglevel", "error", "-y",
            "-f", "lavfi", "-i", "testsrc=duration=3:size=640x360:rate=30"];
        if audio {
            args.extend(["-f", "lavfi", "-i", "sine=frequency=440:duration=3"]);
        }
        args.extend(["-c:v", vcodec, "-pix_fmt", "yuv420p", "-bf", "3", "-g", "30"]);
        if audio {
            args.extend(["-c:a", "aac", "-shortest"]);
        }
        args.extend(["-f", "mpegts", path.to_str()?]);
        run("ffmpeg", &args)?;
        Some(path)
    }

    fn probe(mp4: &Path, args: &[&str]) -> Option<String> {
        let mut a = vec!["-hide_banner", "-loglevel", "error"];
        a.extend_from_slice(args);
        a.push(mp4.to_str()?);
        run("ffprobe", &a)
    }

    fn check_case(vcodec: &str, expect_codec: &str, audio: bool) {
        let dir = std::env::temp_dir().join(format!("hghub-remux-test-{}-{}-{}", std::process::id(), vcodec, audio));
        let _ = std::fs::create_dir_all(&dir);
        let Some(ts) = gen_ts(&dir, "in.ts", vcodec, audio) else {
            eprintln!("ffmpeg 不可用或造流失败，跳过 {vcodec}");
            return;
        };
        let mp4 = dir.join("out.mp4");
        let info = remux_ts_to_mp4(&ts, &mp4).expect("换封装应成功");
        assert_eq!(info.width, 640, "宽度");
        assert_eq!(info.height, 360, "高度");
        assert_eq!(info.has_audio, audio, "音频存在性");
        assert!((info.duration_ms as i64 - 3000).abs() < 400, "时长 {} 应接近 3000ms", info.duration_ms);

        let streams = probe(&mp4, &["-show_entries", "stream=codec_name", "-of", "csv=p=0"]).expect("ffprobe 失败");
        assert!(streams.contains(expect_codec), "ffprobe 应报告 {expect_codec}，实际：{streams}");
        if audio {
            assert!(streams.contains("aac"), "ffprobe 应报告 aac，实际：{streams}");
        }
        // 解码全程无错误 + 帧数一致（3s × 30fps = 90）
        let frames = probe(&mp4, &["-select_streams", "v:0", "-count_frames", "-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"])
            .expect("ffprobe 数帧失败");
        assert!(frames.trim().starts_with("90"), "应有 90 帧，实际：{frames}");
        let dec = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i", mp4.to_str().unwrap(), "-f", "null", "-"])
            .output()
            .expect("ffmpeg 解码失败");
        assert!(dec.status.success(), "ffmpeg 解码 MP4 报错：{}", String::from_utf8_lossy(&dec.stderr));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remux_h264_aac() {
        check_case("libx264", "h264", true);
    }

    #[test]
    fn remux_h264_no_audio() {
        check_case("libx264", "h264", false);
    }

    #[test]
    fn remux_h265_aac() {
        check_case("libx265", "hevc", true);
    }
}

