//! 只读解析 Chromium 系浏览器 localStorage 的 LevelDB(design.md §2.5.2)。
//!
//! kimi（www.kimi.com，Next.js SPA）不走 cookie 认证：登录 token 存在
//! localStorage（如 `access_token`），必须从浏览器的
//! `{profile}/Local Storage/leveldb/` 导入后注入 fama 的 WKWebView。
//!
//! 存储格式（Chromium master `cached_storage_area.cc` 的 `StorageFormat` +
//! 本机 Edge 156 实测双重确认）：
//! - LevelDB 布局：`.ldb`（SSTable）+ `.log`（WAL），block 可能
//!   none / snappy / zstd 压缩（Edge 156 实测 zstd）；
//! - user key = `'_' + origin + '\x00' + <1B key flag> + key`，
//!   value = `<1B flag> + payload`；flag：`0x00`=UTF-16LE、`0x01`=Latin-1
//!   （社区流传的 "\x00\x01 scheme-id v2" 说法实为 Latin1 flag）；
//! - `META:` / `METAACCESS:` 前缀键是 Chromium 元数据（protobuf），不导入；
//! - 第三方分区 origin 形如 `scheme://host/^N<site>`，只匹配第一方 origin。
//!
//! 为什么手写而不用 rusty-leveldb：该库依赖的 integer-encoding 1.1.7 在
//! aarch64 上对 internal key tag 做未对齐指针解引用（真实数据必崩），且
//! 不支持 zstd block（报 snappy corrupt）。本模块只实现「遍历全部键值」
//! 所需的最小只读路径：footer → index block → data blocks + WAL 重组，
//! 序列号大者胜、删除标记生效，等价于 LevelDB 的最新值语义。
//!
//! 数据安全：本模块只返回解码后的键值，不打印（日志侧仅键名与长度，
//! 见 browser_session 的 ilog 约定）；对损坏 block / 记录逐个跳过不中止
//! （浏览器运行中写入造成的 torn read 表现为个别键缺失，可重试）。

use std::collections::HashMap;
use std::path::Path;

// ---------- varint ----------

/// 读一个 LEB128 varint;越界返回 None
fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let b = *buf.get(*pos)?;
        *pos += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

// 仅测试构造器使用（主解析路径只读）
#[cfg(test)]
fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

// ---------- block ----------

/// 解析一个 block 的全部 entry（前缀压缩展开）。
/// 布局：entries + restarts[u32]*n + n_restarts(u32)。
/// 返回 (internal key, value)；损坏时返回已解析出的前缀部分。
fn parse_block_entries(data: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    if data.len() < 4 {
        return out;
    }
    let n_restarts = u32::from_le_bytes([
        data[data.len() - 4],
        data[data.len() - 3],
        data[data.len() - 2],
        data[data.len() - 1],
    ]) as usize;
    let restarts_off = data.len().saturating_sub(4 + 4 * n_restarts);
    let mut pos = 0usize;
    let mut prev_key: Vec<u8> = Vec::new();
    while pos < restarts_off {
        let shared = match read_varint(data, &mut pos) {
            Some(v) => v as usize,
            None => break,
        };
        let non_shared = match read_varint(data, &mut pos) {
            Some(v) => v as usize,
            None => break,
        };
        let val_len = match read_varint(data, &mut pos) {
            Some(v) => v as usize,
            None => break,
        };
        if shared > prev_key.len() || pos + non_shared + val_len > restarts_off {
            break; // 损坏 / 越界：放弃本 block 剩余部分
        }
        let mut key = prev_key[..shared].to_vec();
        key.extend_from_slice(&data[pos..pos + non_shared]);
        pos += non_shared;
        let val = data[pos..pos + val_len].to_vec();
        pos += val_len;
        out.push((key.clone(), val));
        prev_key = key;
    }
    out
}

/// 读 (offset, len) 处的 block 并按压缩类型解压。
/// LevelDB 类型字节：0=none 1=snappy；Chromium 构建另有 zstd（无独立类型号，
/// Edge 156 实测 ctype=1 却是 zstd 流——按 snappy 失败后试 zstd 魔数兜底）。
fn read_block(file: &[u8], offset: usize, len: usize) -> Option<Vec<u8>> {
    if offset + len + 5 > file.len() {
        return None;
    }
    let raw = &file[offset..offset + len];
    let ctype = file[offset + len];
    match ctype {
        0 => Some(raw.to_vec()),
        _ => snap::raw::Decoder::new()
            .decompress_vec(raw)
            .ok()
            .or_else(|| decode_zstd(raw)),
    }
}

/// zstd 帧魔数 0x28 B5 2F FD（LE）；非 zstd 流返回 None
fn decode_zstd(raw: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    if raw.len() >= 4 && raw[..4] == [0x28, 0xb5, 0x2f, 0xfd] {
        let mut out = Vec::new();
        ruzstd::decoding::StreamingDecoder::new(raw)
            .ok()?
            .read_to_end(&mut out)
            .ok()?;
        Some(out)
    } else {
        None
    }
}

// ---------- SSTable ----------

const SSTABLE_MAGIC: u64 = 0xdb47_7524_8b80_fb57;

/// 解析单个 `.ldb` 文件，返回全部 (internal key, value)。
/// internal key = user key + 8B tag（LE u64：低 8 位类型，高 56 位序列号）。
/// footer / index 损坏时返回空（调用方按「该文件无数据」处理）。
fn parse_sstable(file: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    if file.len() < 48 {
        return out;
    }
    let footer = &file[file.len() - 48..];
    let magic = u64::from_le_bytes(footer[40..48].try_into().unwrap());
    if magic != SSTABLE_MAGIC {
        return out;
    }
    let mut pos = 0usize;
    // 两个 BlockHandle（metaindex、index），只取 index 的
    let (Some(_meta_off), Some(_meta_len)) =
        (read_varint(footer, &mut pos), read_varint(footer, &mut pos))
    else {
        return out;
    };
    let (Some(idx_off), Some(idx_len)) =
        (read_varint(footer, &mut pos), read_varint(footer, &mut pos))
    else {
        return out;
    };
    let Some(index) = read_block(file, idx_off as usize, idx_len as usize) else {
        return out;
    };
    for (_, handle) in parse_block_entries(&index) {
        let mut hp = 0usize;
        let (Some(off), Some(len)) = (
            read_varint(&handle, &mut hp),
            read_varint(&handle, &mut hp),
        ) else {
            continue;
        };
        if let Some(block) = read_block(file, off as usize, len as usize) {
            out.extend(parse_block_entries(&block));
        }
    }
    out
}

/// internal key tag 的类型位（1=value、0=delete）
fn ikey_tag(ikey: &[u8]) -> Option<u64> {
    ikey.get(ikey.len() - 8..).map(|t| u64::from_le_bytes(t.try_into().unwrap()))
}

// ---------- WAL ----------

/// 解析 `.log` 文件（32KB block 的 record 流），重组跨 record 的 batch。
/// 返回 (user key, Option<value>, seq)，None = 删除；
/// record header 7B = crc(4) + len(2) + type(1)。
fn parse_wal(file: &[u8]) -> Vec<(Vec<u8>, Option<Vec<u8>>, u64)> {
    let mut out = Vec::new();
    let mut frag: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    while pos + 7 <= file.len() {
        let block_rel = pos % 32768;
        if block_rel + 7 > 32768 {
            pos += 32768 - block_rel; // 尾部不足 header 的 padding
            continue;
        }
        let len = u16::from_le_bytes(file[pos + 4..pos + 6].try_into().unwrap()) as usize;
        let rtype = file[pos + 6];
        pos += 7;
        if pos + len > file.len() {
            break;
        }
        match rtype {
            1 => {
                // FULL：完整 batch
                frag.clear();
                frag.extend_from_slice(&file[pos..pos + len]);
                out.extend(parse_batch(&frag));
            }
            2..=4 => {
                // FIRST / MIDDLE / LAST 分片
                frag.extend_from_slice(&file[pos..pos + len]);
                if rtype == 4 {
                    out.extend(parse_batch(&frag));
                    frag.clear();
                }
            }
            _ => {} // 0 = 零填充
        }
        pos += len;
    }
    out
}

/// WriteBatch：[8B seq][4B count] + entries([1B type][varint klen][key][varint vlen][value])
fn parse_batch(batch: &[u8]) -> Vec<(Vec<u8>, Option<Vec<u8>>, u64)> {
    let mut out = Vec::new();
    if batch.len() < 12 {
        return out;
    }
    let seq = u64::from_le_bytes(batch[..8].try_into().unwrap());
    let count = u32::from_le_bytes(batch[8..12].try_into().unwrap()) as usize;
    let mut pos = 12usize;
    for i in 0..count {
        let Some(t) = batch.get(pos).copied() else {
            break;
        };
        pos += 1;
        let Some(klen) = read_varint(batch, &mut pos).map(|v| v as usize) else {
            break;
        };
        if pos + klen > batch.len() {
            break;
        }
        let key = batch[pos..pos + klen].to_vec();
        pos += klen;
        if t == 0 {
            // delete：无 value
            out.push((key, None, seq + i as u64));
            continue;
        }
        let Some(vlen) = read_varint(batch, &mut pos).map(|v| v as usize) else {
            break;
        };
        if pos + vlen > batch.len() {
            break;
        }
        let val = batch[pos..pos + vlen].to_vec();
        pos += vlen;
        out.push((key, Some(val), seq + i as u64));
    }
    out
}

// ---------- localStorage 键值解码 ----------

/// blink `enum StorageFormat : uint8_t { UTF16 = 0, Latin1 = 1 }`
const LS_FMT_UTF16: u8 = 0x00;
const LS_FMT_LATIN1: u8 = 0x01;

/// 解码一个 localStorage 字符串（key 或 value）：1 字节格式前缀 + 负载。
/// 前缀未知（Chromium 读作 corrupt / 空串）→ None。
fn decode_ls_string(raw: &[u8]) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    match raw[0] {
        LS_FMT_LATIN1 => Some(
            raw[1..]
                .iter()
                .map(|&b| b as char) // Latin-1 与 Unicode U+0000..U+00FF 一一对应
                .collect(),
        ),
        LS_FMT_UTF16 => {
            let payload = &raw[1..];
            if !payload.len().is_multiple_of(2) {
                return None;
            }
            let units: Vec<u16> = payload
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    }
}

/// 一条解析出的 localStorage 条目
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LsEntry {
    pub key: String,
    pub value: String,
}

/// localStorage 读取的失败分类（调用方映射到既有错误码体系）
#[derive(Debug)]
pub(crate) enum LsError {
    /// 目录 / 文件不存在（profile 无 localStorage 数据，或文件正被压缩 /
    /// 轮换移走；非错误语义由调用方判断）
    Missing,
    /// 读取文件被权限拒绝（TCC）
    Denied(std::io::Error),
    /// 其他 io 错误
    Io(std::io::Error),
}

impl From<std::io::Error> for LsError {
    fn from(e: std::io::Error) -> Self {
        classify_io_error(e)
    }
}

/// io 错误 → 读取失败分类（目录级与文件级共用）：
/// - NotFound → Missing（跳过：浏览器压缩 / 轮换中，非错误）；
/// - PermissionDenied → Denied（TCC：上报为 E_BROWSER_DATA_TCC_DENIED，
///   绝不当「未登录」处理）；
/// - 其他 → Io（按既定策略上报）。
fn classify_io_error(e: std::io::Error) -> LsError {
    match e.kind() {
        std::io::ErrorKind::NotFound => LsError::Missing,
        std::io::ErrorKind::PermissionDenied => LsError::Denied(e),
        _ => LsError::Io(e),
    }
}

/// 读取一个 leveldb 文件并分类失败（SSTable 与 WAL 共用）。
/// 单文件 NotFound 跳过（Missing）；权限拒绝 / 其他 IO 错误**上报**，
/// 不再静默当跳过——否则 TCC 缺权会被误报成「未登录」。
fn read_leveldb_file(path: &Path) -> Result<Vec<u8>, LsError> {
    std::fs::read(path).map_err(classify_io_error)
}

/// 列出目录下指定扩展名的文件（按路径排序）。read_dir 本身与目录项迭代
/// 错误都显式传播（不再 filter_map 静默丢弃）。
fn list_dir_files(dir: &Path, exts: &[&str]) -> Result<Vec<std::path::PathBuf>, LsError> {
    let mut out: Vec<std::path::PathBuf> = std::fs::read_dir(dir)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|e| exts.iter().any(|x| e == std::ffi::OsStr::new(x)))
        })
        .collect();
    out.sort();
    Ok(out)
}

/// 读取 leveldb 目录中指定 origin（如 `https://www.kimi.com`）的全部
/// localStorage 条目。按 LevelDB 最新值语义合并（seq 大者胜、删除生效），
/// 只取第一方 origin（`_` + origin + `\x00` 前缀），META/METAACCESS 与
/// 第三方分区键天然不命中。返回按 key 排序。
pub(crate) fn read_origin_entries(dir: &Path, origin: &str) -> Result<Vec<LsEntry>, LsError> {
    let prefix = format!("_{origin}\0").into_bytes();

    // (user key → (value, seq, deleted))，seq 大者胜
    let mut merged: HashMap<Vec<u8>, (Vec<u8>, u64, bool)> = HashMap::new();
    let mut apply = |user_key: Vec<u8>, val: Vec<u8>, seq: u64, deleted: bool| {
        let e = merged.entry(user_key).or_insert((Vec::new(), 0, false));
        if seq >= e.1 {
            *e = (val, seq, deleted);
        }
    };

    // SSTable 按文件名（文件号）升序
    for path in list_dir_files(dir, &["ldb", "sst"])? {
        let file = match read_leveldb_file(&path) {
            Ok(f) => f,
            // NotFound：浏览器压缩 / 轮换中移走了文件，跳过
            Err(LsError::Missing) => continue,
            // 权限拒绝 / 其他 IO：上报（TCC 缺权不得误报「未登录」）
            Err(e) => return Err(e),
        };
        for (ikey, val) in parse_sstable(&file) {
            if ikey.len() < 8 {
                continue;
            }
            let Some(tag) = ikey_tag(&ikey) else {
                continue;
            };
            apply(
                ikey[..ikey.len() - 8].to_vec(),
                val,
                tag >> 8,
                (tag & 0xff) == 0,
            );
        }
    }

    // WAL 的 seq 全局递增且只在 flush 后才删，最新写入总在 .log 里
    for path in list_dir_files(dir, &["log"])? {
        let file = match read_leveldb_file(&path) {
            Ok(f) => f,
            Err(LsError::Missing) => continue,
            Err(e) => return Err(e),
        };
        for (user_key, val, seq) in parse_wal(&file) {
            // Some=put（空串值也合法）、None=delete
            let (val, deleted) = match val {
                Some(v) => (v, false),
                None => (Vec::new(), true),
            };
            apply(user_key, val, seq, deleted);
        }
    }

    let mut out: Vec<LsEntry> = merged
        .into_iter()
        .filter(|(k, (_, _, deleted))| {
            !*deleted && k.len() > prefix.len() && k.starts_with(&prefix)
        })
        .filter_map(|(k, (v, _, _))| {
            // k = prefix + <1B key flag> + key；v = <1B flag> + payload
            let key = decode_ls_string(&k[prefix.len()..])?;
            let value = decode_ls_string(&v)?;
            Some(LsEntry { key, value })
        })
        .collect();
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定 zstd 压缩向量：zstd CLI 对 `build_block`（2 条 entry：
    /// `_https://www.kimi.com\0\x01k` → `\x01v`、`zz` → `\x01w`）输出的
    /// 压缩流（77 字节，magic `28 b5 2f fd`）。内嵌后测试与机器是否安装
    /// zstd CLI 无关。
    const ZSTD_BLOCK_FIXTURE: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x58, 0x01, 0x02, 0x00, 0x00, 0x20, 0x02, 0x5f, 0x68,
        0x74, 0x74, 0x70, 0x73, 0x3a, 0x2f, 0x2f, 0x77, 0x77, 0x77, 0x2e, 0x6b, 0x69, 0x6d,
        0x69, 0x2e, 0x63, 0x6f, 0x6d, 0x00, 0x01, 0x6b, 0x01, 0x05, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x01, 0x76, 0x00, 0x0a, 0x02, 0x7a, 0x7a, 0x01, 0x06, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x01, 0x77, 0x00, 0x00, 0x00, 0x00, 0x25, 0x00, 0x00, 0x00, 0x02,
        0x00, 0x00, 0x00, 0x2d, 0x73, 0x64, 0x64,
    ];

    // ---------- 测试构造器：block / SSTable / WAL ----------

    /// 构造一个 block（restart interval = 1，即每条 entry 都是 restart 点、
    /// 全 key 存储——解析器不依赖 interval，只要 restarts 表与 entries 对齐）
    fn build_block(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        let mut restarts: Vec<u32> = Vec::new();
        for (k, v) in entries {
            restarts.push(body.len() as u32);
            write_varint(&mut body, 0); // shared = 0
            write_varint(&mut body, k.len() as u64);
            write_varint(&mut body, v.len() as u64);
            body.extend_from_slice(k);
            body.extend_from_slice(v);
        }
        for r in &restarts {
            body.extend_from_slice(&r.to_le_bytes());
        }
        body.extend_from_slice(&(restarts.len() as u32).to_le_bytes());
        body
    }

    /// 把 block 按 32 位绝对偏移切分（让 entry 键做前缀压缩的跨 block 场景不出现；
    /// restart_interval=1 时 shared 恒 0）
    fn block_bytes_with_restarts(entries: &[(&[u8], &[u8])], shared_from: usize) -> Vec<u8> {
        // shared_from: 前多少字节与上一个 key 相同（测试前缀压缩路径）
        let mut body = Vec::new();
        let mut restarts: Vec<u32> = Vec::new();
        for (i, (k, v)) in entries.iter().enumerate() {
            if i % 2 == 0 {
                restarts.push(body.len() as u32);
            }
            let shared = if i % 2 == 0 { 0 } else { shared_from.min(k.len()) };
            write_varint(&mut body, shared as u64);
            write_varint(&mut body, (k.len() - shared) as u64);
            write_varint(&mut body, v.len() as u64);
            if i % 2 == 0 {
                body.extend_from_slice(k);
            } else {
                body.extend_from_slice(&k[shared..]);
            }
            body.extend_from_slice(v);
        }
        for r in &restarts {
            body.extend_from_slice(&r.to_le_bytes());
        }
        body.extend_from_slice(&(restarts.len() as u32).to_le_bytes());
        body
    }

    /// internal key：user key + 8B tag（type 在低 8 位）
    fn ikey(user: &[u8], seq: u64, deleted: bool) -> Vec<u8> {
        let mut k = user.to_vec();
        let tag = (seq << 8) | if deleted { 0 } else { 1 };
        k.extend_from_slice(&tag.to_le_bytes());
        k
    }

    /// 构造一个完整 SSTable 文件（不压缩）：data blocks + index block + footer。
    /// entry key 自动加 internal tag（seq 按 block/entry 递增，便于测试用
    /// 低 seq 的 SSTable + 高 seq 的 WAL 表达覆盖/删除语义）。
    pub(crate) fn build_sstable(blocks: &[Vec<(Vec<u8>, Vec<u8>)>]) -> Vec<u8> {
        let mut file = Vec::new();
        let mut index_entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for (bi, entries) in blocks.iter().enumerate() {
            let tagged: Vec<(Vec<u8>, Vec<u8>)> = entries
                .iter()
                .enumerate()
                .map(|(i, (k, v))| (ikey(k, bi as u64 * 1000 + i as u64, false), v.clone()))
                .collect();
            let block = build_block(
                &tagged
                    .iter()
                    .map(|(k, v)| (k.as_slice(), v.as_slice()))
                    .collect::<Vec<_>>(),
            );
            let offset = file.len();
            // block + 1B 压缩类型(0=none) + 4B crc（crc 解析时不校验，填 0）
            file.extend_from_slice(&block);
            file.push(0);
            file.extend_from_slice(&[0, 0, 0, 0]);
            let mut handle = Vec::new();
            write_varint(&mut handle, offset as u64);
            write_varint(&mut handle, block.len() as u64);
            // index entry 的 key 只需 ≥ 该 block 最后一个 key（用最后 key 本身）
            let last_key = tagged.last().map(|(k, _)| k.clone()).unwrap_or_default();
            index_entries.push((ikey(&last_key, u64::MAX - 1, false), handle));
        }
        let idx_offset = file.len();
        let index_block = build_block(
            &index_entries
                .iter()
                .map(|(k, v)| (k.as_slice(), v.as_slice()))
                .collect::<Vec<_>>(),
        );
        file.extend_from_slice(&index_block);
        file.push(0);
        file.extend_from_slice(&[0, 0, 0, 0]);

        // footer：两个 handle + padding 到 40B + 8B magic
        let mut footer = Vec::new();
        write_varint(&mut footer, 0); // metaindex（不需要，占位）
        write_varint(&mut footer, 0);
        write_varint(&mut footer, idx_offset as u64);
        write_varint(&mut footer, index_block.len() as u64);
        while footer.len() < 40 {
            footer.push(0);
        }
        footer.extend_from_slice(&SSTABLE_MAGIC.to_le_bytes());
        assert_eq!(footer.len(), 48);
        file.extend_from_slice(&footer);
        file
    }

    /// 构造 WAL 文件：每 batch 一条 FULL record（不跨 32KB 边界）
    pub(crate) fn build_wal(batches: &[&[(Vec<u8>, Vec<u8>)]], base_seq: u64) -> Vec<u8> {
        let mut file = Vec::new();
        for (i, batch) in batches.iter().enumerate() {
            let mut b = Vec::new();
            b.extend_from_slice(&(base_seq + (i as u64) * 100).to_le_bytes());
            b.extend_from_slice(&(batch.len() as u32).to_le_bytes());
            for (k, v) in batch.iter() {
                if v.is_empty() {
                    b.push(0); // delete
                } else {
                    b.push(1); // put
                }
                write_varint(&mut b, k.len() as u64);
                b.extend_from_slice(k);
                if !v.is_empty() {
                    write_varint(&mut b, v.len() as u64);
                    b.extend_from_slice(v);
                }
            }
            let crc = 0u32; // 不校验
            file.extend_from_slice(&crc.to_le_bytes());
            file.extend_from_slice(&(b.len() as u16).to_le_bytes());
            file.push(1); // FULL
            file.extend_from_slice(&b);
        }
        file
    }

    // localStorage 编码（写入侧，与 blink StringToUint8Vector 对齐）
    pub(crate) fn ls_encode_latin1(s: &str) -> Vec<u8> {
        let mut v = vec![LS_FMT_LATIN1];
        v.extend(s.chars().map(|c| c as u8));
        v
    }

    pub(crate) fn ls_encode_utf16(s: &str) -> Vec<u8> {
        let mut v = vec![LS_FMT_UTF16];
        for u in s.encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v
    }

    pub(crate) fn ls_key(origin: &str, key: &str) -> Vec<u8> {
        let mut k = format!("_{origin}\0").into_bytes();
        k.extend_from_slice(&ls_encode_latin1(key));
        k
    }

    // ---------- varint ----------

    #[test]
    fn varint_roundtrip_and_boundaries() {
        for v in [0u64, 1, 127, 128, 300, 16_383, 16_384, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            write_varint(&mut buf, v);
            let mut pos = 0;
            assert_eq!(read_varint(&buf, &mut pos), Some(v));
            assert_eq!(pos, buf.len());
        }
        // 越界
        let buf = vec![0x80];
        let mut pos = 0;
        assert_eq!(read_varint(&buf, &mut pos), None);
    }

    // ---------- block ----------

    #[test]
    fn block_parses_entries_with_prefix_compression() {
        // 交替 restart（全 key）与非 restart（shared 前缀）
        let k1 = ikey(b"aaa", 1, false);
        let k2 = ikey(b"aab", 2, false);
        let block = block_bytes_with_restarts(&[(k1.as_slice(), b"v1"), (k2.as_slice(), b"v2")], 2);
        let entries = parse_block_entries(&block);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], (k1, b"v1".to_vec()));
        assert_eq!(entries[1], (k2, b"v2".to_vec()));
    }

    #[test]
    fn block_truncated_returns_partial() {
        let k = ikey(b"key", 1, false);
        let block = build_block(&[(k.as_slice(), b"value")]);
        let cut = &block[..block.len() - 3]; // 尾部截断
        let entries = parse_block_entries(cut);
        assert!(entries.is_empty() || entries.len() == 1); // 不 panic 即可
    }

    // ---------- 解码 ----------

    #[test]
    fn decode_ls_string_formats() {
        assert_eq!(decode_ls_string(&ls_encode_latin1("access_token")), Some("access_token".into()));
        // Latin-1 非 ASCII：é = 0xE9
        assert_eq!(decode_ls_string(&[LS_FMT_LATIN1, 0xE9]), Some("é".into()));
        // UTF-16：中文
        let s = "你好";
        assert_eq!(decode_ls_string(&ls_encode_utf16(s)), Some(s.to_string()));
        // 未知前缀（corrupt）→ None
        assert_eq!(decode_ls_string(&[0x08, b'x']), None);
        // 空 → None
        assert_eq!(decode_ls_string(&[]), None);
        // UTF-16 奇数长度负载 → None
        assert_eq!(decode_ls_string(&[LS_FMT_UTF16, 0x41, 0x00, 0x42]), None);
    }

    // ---------- SSTable / WAL 端到端 ----------

    #[test]
    fn sstable_roundtrip_multiple_blocks() {
        let b1 = vec![
            (ls_key("https://www.kimi.com", "access_token"), ls_encode_latin1("tok-abc")),
            (ls_key("https://www.kimi.com", "theme"), ls_encode_latin1("dark")),
        ];
        let b2 = vec![
            (ls_key("https://chat.deepseek.com", "api"), ls_encode_utf16("中文值")),
            (b"VERSION".to_vec(), b"1".to_vec()),
        ];
        let file = build_sstable(&[b1.clone(), b2.clone()]);
        // parse_sstable 返回 internal key（user key + 8B tag），剥 tag 后比较
        let mut got: Vec<(Vec<u8>, Vec<u8>)> = parse_sstable(&file)
            .into_iter()
            .map(|(ik, v)| (ik[..ik.len() - 8].to_vec(), v))
            .collect();
        got.sort();
        let mut want: Vec<(Vec<u8>, Vec<u8>)> = b1.into_iter().chain(b2).collect();
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn sstable_rejects_bad_magic() {
        let mut file = build_sstable(&[vec![(b"k".to_vec(), b"v".to_vec())]]);
        let n = file.len();
        file[n - 1] ^= 0xff; // 破坏 magic
        assert!(parse_sstable(&file).is_empty());
    }

    #[test]
    fn wal_roundtrip_and_seq_semantics() {
        let b1 = vec![(ls_key("https://www.kimi.com", "a"), ls_encode_latin1("1"))];
        let b2 = vec![(ls_key("https://www.kimi.com", "b"), ls_encode_latin1("2"))];
        let batches: Vec<&[(Vec<u8>, Vec<u8>)]> = vec![&b1, &b2];
        let file = build_wal(&batches, 1000);
        let got = parse_wal(&file);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].2, 1000);
        assert_eq!(got[1].2, 1100);
    }

    #[test]
    fn zstd_magic_detection() {
        assert!(decode_zstd(&[]).is_none());
        assert!(decode_zstd(&[0x28, 0xb5]).is_none());
        assert!(decode_zstd(&[0x00, 0x00, 0x00, 0x00]).is_none());
        // 非 zstd 的 snappy 数据不会被误判
        let snappy = snap::raw::Encoder::new()
            .compress_vec(b"hello snappy world")
            .unwrap();
        assert!(decode_zstd(&snappy).is_none());
    }

    // ---------- read_origin_entries 集成 ----------

    struct TmpDir(std::path::PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn make_dir(tag: &str) -> TmpDir {
        let p = std::env::temp_dir().join(format!(
            "fama-test-ls-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }

    #[test]
    fn read_origin_entries_end_to_end() {
        let tmp = make_dir("e2e");
        let origin = "https://www.kimi.com";
        // .ldb：3 条 kimi（1 条被 WAL 覆盖）+ 1 条其他 origin + 1 条 META + 1 条已删除
        let sst_blocks = vec![vec![
            (ls_key(origin, "access_token"), ls_encode_latin1("old-tok")),
            (ls_key(origin, "msh_user_id"), ls_encode_utf16("12345")),
            (ls_key("https://chat.deepseek.com", "x"), ls_encode_latin1("y")),
            (format!("META:{origin}").into_bytes(), b"\x08\x01".to_vec()),
            (ls_key(origin, "stale"), ls_encode_latin1("gone")), // seq 低 + 稍后删除
        ]];
        std::fs::write(tmp.0.join("000005.ldb"), build_sstable(&sst_blocks)).unwrap();
        // .log：覆盖 access_token + 删除 stale + 新增 theme
        let wal_batch = vec![
            (ls_key(origin, "access_token"), ls_encode_latin1("new-tok")),
            (ls_key(origin, "stale"), Vec::new()), // delete（build_wal 编码为 type 0）
            (ls_key(origin, "theme"), ls_encode_latin1("dark")),
        ];
        let wal_batches: Vec<&[(Vec<u8>, Vec<u8>)]> = vec![&wal_batch];
        std::fs::write(tmp.0.join("000006.log"), build_wal(&wal_batches, 10_000)).unwrap();

        let entries = read_origin_entries(&tmp.0, origin).unwrap();
        let find = |k: &str| entries.iter().find(|e| e.key == k).map(|e| e.value.clone());
        assert_eq!(entries.len(), 3);
        assert_eq!(find("access_token").as_deref(), Some("new-tok")); // WAL 覆盖 SSTable
        assert_eq!(find("msh_user_id").as_deref(), Some("12345"));
        assert_eq!(find("theme").as_deref(), Some("dark"));
        assert!(find("stale").is_none()); // 已删除
        assert!(find("x").is_none()); // 其他 origin
        // META 键不出现（前缀不匹配）
        assert!(!entries.iter().any(|e| e.key.starts_with("META")));
    }

    #[test]
    fn read_origin_entries_missing_dir() {
        let tmp = make_dir("missing");
        let err = read_origin_entries(&tmp.0.join("nope"), "https://www.kimi.com").unwrap_err();
        assert!(matches!(err, LsError::Missing));
    }

    #[test]
    fn read_origin_entries_denied_maps_to_denied() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = make_dir("denied");
        std::fs::write(tmp.0.join("000005.ldb"), b"x").unwrap();
        std::fs::set_permissions(&tmp.0, std::fs::Permissions::from_mode(0o000)).unwrap();
        let res = read_origin_entries(&tmp.0, "https://www.kimi.com");
        std::fs::set_permissions(&tmp.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        match res {
            Err(LsError::Denied(_)) => {}
            // root 环境不受 mode 位约束,跳过
            Ok(_) => {}
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    /// classify_io_error 纯分类:EPERM/EACCES → Denied,ENOENT → Missing,
    /// 其他(EIO)→ Io。不依赖 OS 实际拒绝,任何环境都验证分支本身。
    #[test]
    fn classify_io_error_categories() {
        assert!(matches!(
            classify_io_error(std::io::Error::from_raw_os_error(1)), // EPERM
            LsError::Denied(_)
        ));
        assert!(matches!(
            classify_io_error(std::io::Error::from_raw_os_error(13)), // EACCES
            LsError::Denied(_)
        ));
        assert!(matches!(
            classify_io_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            LsError::Denied(_)
        ));
        assert!(matches!(
            classify_io_error(std::io::Error::from_raw_os_error(2)), // ENOENT
            LsError::Missing
        ));
        assert!(matches!(
            classify_io_error(std::io::Error::from(std::io::ErrorKind::NotFound)),
            LsError::Missing
        ));
        assert!(matches!(
            classify_io_error(std::io::Error::from_raw_os_error(5)), // EIO
            LsError::Io(_)
        ));
    }

    /// 单个 .ldb 文件权限拒绝 → Denied,不得静默跳过(TCC 缺权被误报为
    /// 「未登录」的正是这条路径)。root 下 chmod 不生效则显式跳过。
    #[test]
    fn read_origin_entries_single_ldb_denied() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = make_dir("ldb-denied");
        let ldb = tmp.0.join("000005.ldb");
        std::fs::write(&ldb, b"x").unwrap();
        std::fs::set_permissions(&ldb, std::fs::Permissions::from_mode(0o000)).unwrap();
        // root 预检:mode 000 仍可读说明 chmod 无效(root),不宣称 OS 拒绝已验证
        if std::fs::read(&ldb).is_ok() {
            std::fs::set_permissions(&ldb, std::fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }
        let res = read_origin_entries(&tmp.0, "https://www.kimi.com");
        std::fs::set_permissions(&ldb, std::fs::Permissions::from_mode(0o644)).unwrap();
        match res {
            Err(LsError::Denied(_)) => {}
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    /// 单个 .log(WAL)文件权限拒绝 → Denied,同上
    #[test]
    fn read_origin_entries_single_log_denied() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = make_dir("log-denied");
        let log = tmp.0.join("000006.log");
        std::fs::write(&log, b"x").unwrap();
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&log).is_ok() {
            std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }
        let res = read_origin_entries(&tmp.0, "https://www.kimi.com");
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
        match res {
            Err(LsError::Denied(_)) => {}
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    /// zstd 压缩 block 的真实解压。向量由 zstd CLI 一次性生成后内嵌为固定
    /// 字节（77 字节），测试不再依赖机器预装 zstd，也不静默跳过。
    #[test]
    fn zstd_block_roundtrip() {
        let z = ZSTD_BLOCK_FIXTURE;
        let dec = decode_zstd(z).expect("zstd decode");
        let plain = build_block(&[
            (ikey(b"_https://www.kimi.com\0\x01k", 5, false).as_slice(), b"\x01v"),
            (ikey(b"zz", 6, false).as_slice(), b"\x01w"),
        ]);
        assert_eq!(dec, plain);
        let entries = parse_block_entries(&dec);
        assert_eq!(entries.len(), 2);
    }

    /// snappy 压缩 block 的解压路径(read_block ctype=1)
    #[test]
    fn snappy_block_via_read_block() {
        let plain = build_block(&[(ikey(b"kk", 1, false).as_slice(), b"vv")]);
        let compressed = snap::raw::Encoder::new().compress_vec(&plain).unwrap();
        let mut file = compressed.clone();
        file.push(1); // snappy
        file.extend_from_slice(&[0, 0, 0, 0]); // crc
        let block = read_block(&file, 0, compressed.len()).unwrap();
        assert_eq!(block, plain);
    }

    /// read_origin_entries 只匹配完整 origin 前缀:`_origin\0` 不会误命中
    /// 同前缀字符串的其他 origin(如 www.kimi.com.evil.com)
    #[test]
    fn origin_prefix_no_false_positive() {
        let tmp = make_dir("prefix");
        let blocks = vec![vec![(
            ls_key("https://www.kimi.com.evil.com", "access_token"),
            ls_encode_latin1("evil"),
        )]];
        std::fs::write(tmp.0.join("000005.ldb"), build_sstable(&blocks)).unwrap();
        let entries = read_origin_entries(&tmp.0, "https://www.kimi.com").unwrap();
        assert!(entries.is_empty());
    }
}

/// 跨模块测试支持：browser_session 的 scan_local_storage 集成测试需要构造
/// 真实 leveldb 目录（SSTable / WAL / localStorage 编码）。仅测试构建可见。
#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) use super::tests::{build_sstable, ls_encode_latin1, ls_encode_utf16, ls_key};
}
