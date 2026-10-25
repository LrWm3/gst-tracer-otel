//! File-based profile symbolization. Never calls into the dynamic loader.
use pyroscope::backend::StackFrame;
use std::{collections::HashMap, fs::File, os::unix::fs::MetadataExt, path::PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Symbolization {
    #[default]
    Full,
    Functions,
}
impl std::str::FromStr for Symbolization {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "full" => Ok(Self::Full),
            "functions" => Ok(Self::Functions),
            _ => Err("profile SYMBOLIZATION must be full or functions".into()),
        }
    }
}
const MAX_SYMBOLS: usize = 4096;
const SYMBOL_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Mapping {
    start: usize,
    end: usize,
    offset: u64,
    device: u64,
    inode: u64,
    path: PathBuf,
}
impl Mapping {
    fn parse(line: &str) -> Option<Self> {
        let mut rest = line;
        let mut field = || {
            rest = rest.trim_start();
            let (value, remaining) =
                rest.split_at(rest.find(char::is_whitespace).unwrap_or(rest.len()));
            rest = remaining;
            value
        };
        let (start, end) = field().split_once('-')?;
        if !field().contains('x') {
            return None;
        }
        let offset = u64::from_str_radix(field(), 16).ok()?;
        let (major, minor) = field().split_once(':')?;
        let device = libc::makedev(
            u32::from_str_radix(major, 16).ok()?,
            u32::from_str_radix(minor, 16).ok()?,
        );
        let inode = field().parse().ok()?;
        let path = rest.trim_start();
        if !path.starts_with('/') || path.ends_with(" (deleted)") {
            return None;
        }
        // proc escapes newlines; spaces (including consecutive spaces) are literal.
        let path = PathBuf::from(path.replace("\\012", "\n"));
        Some(Self {
            start: usize::from_str_radix(start, 16).ok()?,
            end: usize::from_str_radix(end, 16).ok()?,
            offset,
            device,
            inode,
            path,
        })
    }
    fn open(&self) -> Option<File> {
        let file = File::open(&self.path).ok()?;
        let metadata = file.metadata().ok()?;
        // Do not symbolize against a replacement file at the same path.
        (metadata.ino() == self.inode && metadata.dev() == self.device).then_some(file)
    }
}

#[derive(Default)]
pub(super) struct Symbols {
    mappings: Vec<Mapping>,
    pub frames: HashMap<usize, Vec<StackFrame>>,
    bytes: usize,
    mode: Symbolization,
    generation: u64,
    used: HashMap<usize, u64>,
}
impl Symbols {
    pub fn new(mode: Symbolization) -> Self {
        Self {
            mode,
            ..Default::default()
        }
    }

    fn remove(&mut self, pc: usize) {
        if let Some(frames) = self.frames.remove(&pc) {
            self.bytes -= frame_bytes(&frames);
            self.used.remove(&pc);
        }
    }

    fn mappings(&mut self, mappings: Vec<Mapping>) -> usize {
        fn containing(mappings: &[Mapping], pc: usize) -> Option<&Mapping> {
            mappings
                .partition_point(|m| m.start <= pc)
                .checked_sub(1)
                .and_then(|i| mappings.get(i))
                .filter(|m| pc < m.end)
        }
        let stale: Vec<_> = self
            .frames
            .keys()
            .copied()
            .filter(|&pc| {
                let old = containing(&self.mappings, pc);
                old.is_none() || old != containing(&mappings, pc)
            })
            .collect();
        for &pc in &stale {
            self.remove(pc);
        }
        self.mappings = mappings;
        stale.len()
    }

    pub fn refresh(&mut self, pcs: &[usize]) {
        let mut evicted = 0;
        let mappings: Vec<_> = std::fs::read_to_string("/proc/self/maps")
            .unwrap_or_default()
            .lines()
            .filter_map(Mapping::parse)
            .collect();
        if self.mappings != mappings {
            evicted += self.mappings(mappings);
        }
        self.generation = self.generation.saturating_add(1);
        for pc in pcs.iter().filter(|pc| self.frames.contains_key(pc)) {
            self.used.insert(*pc, self.generation);
        }
        // Keep this report's cached PCs. Retire least recently used entries
        // absent from the report, without flushing unrelated hot symbols.
        let mut stale: Vec<_> = self
            .used
            .iter()
            .filter(|(pc, _)| pcs.binary_search(pc).is_err())
            .map(|(&pc, &age)| (age, pc))
            .collect();
        stale.sort_unstable_by(|a, b| b.cmp(a));
        let missing = pcs
            .iter()
            .filter(|pc| !self.frames.contains_key(pc))
            .count();
        while self.frames.len() + missing > MAX_SYMBOLS {
            let Some((_, pc)) = stale.pop() else { break };
            self.remove(pc);
            evicted += 1;
        }
        let hits = if gst_mars_metrics_core::monitoring::level()
            == gst_mars_metrics_core::monitoring::Monitoring::All
        {
            pcs.iter().filter(|pc| self.frames.contains_key(pc)).count()
        } else {
            0
        };
        // PCs are sorted, so each ELF/DWARF context is built once per report
        // and dropped before loading the next file. No global resolver lock,
        // libunwind, dladdr, or dl_iterate_phdr (which takes the loader lock).
        for mapping in self.mappings.clone() {
            let first = pcs.partition_point(|pc| *pc < mapping.start);
            let last = pcs.partition_point(|pc| *pc < mapping.end);
            let missing: Vec<_> = pcs[first..last]
                .iter()
                .copied()
                .filter(|pc| !self.frames.contains_key(pc))
                .collect();
            if missing.is_empty() || self.frames.len() == MAX_SYMBOLS {
                continue;
            }
            let Some(file) = mapping.open() else { continue };
            use object::{Object, ObjectSegment};
            let cache = object::read::ReadCache::new(&file);
            let Ok(object) = object::File::parse(&cache) else {
                continue;
            };
            let segments: Vec<_> = object
                .segments()
                .map(|s| (s.file_range(), s.address()))
                .collect();
            // Prefer the installed build-id debug file when available. The
            // open fd keeps the original inode stable while Loader opens it.
            use std::os::fd::AsRawFd;
            let binary = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
            let debug = (self.mode == Symbolization::Full)
                .then(|| {
                    object
                        .build_id()
                        .ok()
                        .flatten()
                        .filter(|id| id.len() >= 2)
                        .and_then(|id| {
                            let id: String = id.iter().map(|b| format!("{b:02x}")).collect();
                            let path = PathBuf::from(format!(
                                "/usr/lib/debug/.build-id/{}/{}.debug",
                                &id[..2],
                                &id[2..]
                            ));
                            let debug_file = File::open(&path).ok()?;
                            let debug_cache = object::read::ReadCache::new(debug_file);
                            let debug_object = object::File::parse(&debug_cache).ok()?;
                            (debug_object.build_id().ok()? == object.build_id().ok()?)
                                .then_some(path)
                        })
                })
                .flatten();
            let loader = (self.mode == Symbolization::Full)
                .then(|| {
                    debug
                        .as_ref()
                        .and_then(|p| addr2line::Loader::new(p).ok())
                        .or_else(|| addr2line::Loader::new(&binary).ok())
                })
                .flatten();
            let symbols = (self.mode == Symbolization::Functions).then(|| object.symbol_map());
            for pc in missing {
                if self.frames.len() == MAX_SYMBOLS {
                    break;
                }
                let offset = mapping.offset + (pc - mapping.start) as u64;
                let address = segments.iter().find_map(|&((start, len), address)| {
                    let relative = offset.checked_sub(start)?;
                    (relative < len).then(|| address + relative)
                });
                let mut frames = if let Some(symbols) = &symbols {
                    let name =
                        address
                            .and_then(|address| symbols.containing(address))
                            .map(|symbol| {
                                short_string(&addr2line::demangle_auto(symbol.name().into(), None))
                            });
                    vec![StackFrame {
                        name: Some(name.unwrap_or_else(|| format!("0x{pc:x}"))),
                        ..Default::default()
                    }]
                } else {
                    resolve_file(pc, address, loader.as_ref())
                };
                if frames.len() == 1 && frames[0].name.as_deref() == Some(&format!("0x{pc:x}")) {
                    // Keep the instruction address on the pprof location,
                    // but group unnamed code by its mapped library instead
                    // of making ASLR-dependent addresses the function name.
                    frames[0].name = Some(format!(
                        "[unresolved native] {}",
                        mapping
                            .path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                    ));
                }
                let bytes = frame_bytes(&frames);
                while self.bytes + bytes > SYMBOL_BYTES {
                    let Some((_, pc)) = stale.pop() else { break };
                    self.remove(pc);
                    evicted += 1;
                }
                if self.bytes + bytes > SYMBOL_BYTES {
                    continue;
                }
                self.bytes += bytes;
                self.frames.insert(pc, frames);
                self.used.insert(pc, self.generation);
            }
        }
        gst_mars_metrics_core::monitoring::symbol_cache(
            hits,
            pcs.len() - hits,
            evicted,
            self.frames.len(),
            self.bytes,
        );
    }
}

fn frame_bytes(frames: &Vec<StackFrame>) -> usize {
    frames.capacity() * std::mem::size_of::<StackFrame>()
        + frames
            .iter()
            .map(|f| {
                f.name.as_ref().map_or(0, String::capacity)
                    + f.filename.as_ref().map_or(0, String::capacity)
            })
            .sum::<usize>()
}

fn short_string(value: &str) -> String {
    let mut end = value.len().min(1024);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn resolve_file(
    pc: usize,
    address: Option<u64>,
    loader: Option<&addr2line::Loader>,
) -> Vec<StackFrame> {
    let mut frames = Vec::new();
    if let (Some(address), Some(loader)) = (address, loader) {
        if let Ok(mut resolved) = loader.find_frames(address) {
            while let Ok(Some(frame)) = resolved.next() {
                frames.push(StackFrame {
                    name: frame
                        .function
                        .and_then(|f| f.demangle().ok().map(|v| short_string(&v))),
                    filename: frame
                        .location
                        .as_ref()
                        .and_then(|l| l.file.map(short_string)),
                    line: frame.location.and_then(|l| l.line),
                    ..Default::default()
                });
                if frames.len() == 16 {
                    break;
                }
            }
        }
        if frames.is_empty() {
            if let Some(name) = loader.find_symbol(address) {
                frames.push(StackFrame {
                    name: Some(short_string(&addr2line::demangle_auto(name.into(), None))),
                    ..Default::default()
                });
            }
        }
    }
    if frames.is_empty() {
        frames.push(StackFrame {
            name: Some(format!("0x{pc:x}")),
            ..Default::default()
        });
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::c_void, time::Duration};

    #[test]
    fn function_mode_resolves_names_without_source_or_inline_frames() {
        assert_eq!(
            "functions".parse::<Symbolization>().unwrap(),
            Symbolization::Functions
        );
        assert!("fast".parse::<Symbolization>().is_err());
        let pc = resolve_file as *const () as usize;
        let mut symbols = Symbols::new(Symbolization::Functions);
        symbols.refresh(&[pc]);
        let frames = &symbols.frames[&pc];
        assert_eq!(frames.len(), 1);
        assert!(frames[0].name.as_ref().unwrap().contains("resolve_file"));
        assert!(frames[0].filename.is_none() && frames[0].line.is_none());
    }

    fn seed(symbols: &mut Symbols, pc: usize, name: String, age: u64) {
        let frames = vec![StackFrame {
            name: Some(name),
            ..Default::default()
        }];
        symbols.bytes += frame_bytes(&frames);
        symbols.frames.insert(pc, frames);
        symbols.used.insert(pc, age);
    }

    #[test]
    fn unrelated_mapping_changes_keep_cached_symbols_and_replacements_invalidate_them() {
        let original = Mapping::parse("1000-3000 r-xp 00000000 08:01 1 /one.so").unwrap();
        let other = Mapping::parse("4000-5000 r-xp 00000000 08:01 2 /two.so").unwrap();
        let mut symbols = Symbols {
            mappings: vec![original.clone()],
            ..Default::default()
        };
        seed(&mut symbols, 0x1234, "original".into(), 1);
        let pointer = symbols.frames[&0x1234].as_ptr();
        assert_eq!(symbols.mappings(vec![original.clone(), other]), 0);
        assert_eq!(symbols.frames[&0x1234].as_ptr(), pointer);
        let mut replacement = original;
        replacement.inode = 99;
        assert_eq!(symbols.mappings(vec![replacement]), 1);
        assert!(symbols.frames.is_empty() && symbols.used.is_empty());
        assert_eq!(symbols.bytes, 0);
    }

    #[test]
    fn capacity_pressure_evicts_stale_entries_without_clearing_the_hot_set() {
        let mut symbols = Symbols::new(Symbolization::Functions);
        symbols.refresh(&[]);
        let pc = resolve_file as *const () as usize;
        let start = symbols
            .mappings
            .iter()
            .find(|m| pc >= m.start && pc < m.end)
            .unwrap()
            .start;
        for offset in 0..MAX_SYMBOLS {
            seed(&mut symbols, start + offset, "cached".into(), offset as u64);
        }
        symbols.generation = MAX_SYMBOLS as u64;
        let hot = start + MAX_SYMBOLS - 1;
        let pointer = symbols.frames[&hot].as_ptr();
        let mut pcs = [hot, pc];
        pcs.sort_unstable();
        assert!(!symbols.frames.contains_key(&pc));
        symbols.refresh(&pcs);
        assert!(symbols.frames.contains_key(&pc));
        assert!(!symbols.frames.contains_key(&start));
        assert_eq!(symbols.frames[&hot].as_ptr(), pointer);
        assert_eq!(symbols.frames.len(), MAX_SYMBOLS);
        assert_eq!(
            symbols.bytes,
            symbols.frames.values().map(frame_bytes).sum::<usize>()
        );
        assert!(symbols.bytes <= SYMBOL_BYTES);
    }

    #[test]
    #[ignore = "manual cold/warm symbolization measurement; use a profiling build"]
    fn symbolization_measurement() {
        let mode = std::env::var("MARS_PROFILE_SYMBOLIZATION").unwrap_or_else(|_| "full".into());
        let mut symbols = Symbols::new(mode.parse().unwrap());
        let mut pcs = vec![
            resolve_file as *const () as usize,
            short_string as *const () as usize,
            frame_bytes as *const () as usize,
            Mapping::parse as *const () as usize,
            libc::malloc as *const () as usize,
            libc::free as *const () as usize,
        ];
        pcs.sort_unstable();
        pcs.dedup();
        fn memory() -> (u64, u64) {
            let status = std::fs::read_to_string("/proc/self/status").unwrap();
            let value = |key| {
                status
                    .lines()
                    .find(|l| l.starts_with(key))
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .parse()
                    .unwrap()
            };
            (value("VmRSS:"), value("VmHWM:"))
        }
        fn cpu() -> u64 {
            let mut value = std::mem::MaybeUninit::uninit();
            // SAFETY: a successful clock_gettime initializes this timespec.
            assert_eq!(
                unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, value.as_mut_ptr()) },
                0
            );
            let value = unsafe { value.assume_init() };
            value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64
        }
        for phase in ["cold", "warm", "cache_cleared"] {
            if phase == "cache_cleared" {
                // Reference cost of the old whole-cache invalidation policy.
                symbols.frames.clear();
                symbols.used.clear();
                symbols.bytes = 0;
            }
            let before = memory();
            let start = std::time::Instant::now();
            let cpu_start = cpu();
            symbols.refresh(&pcs);
            let elapsed = start.elapsed();
            let cpu_ns = cpu() - cpu_start;
            let after = memory();
            assert_eq!(symbols.frames.len(), pcs.len());
            println!("SYMBOLS mode={mode} phase={phase} wall_ns={} cpu_ns={cpu_ns} rss_before_kb={} rss_after_kb={} peak_kb={} entries={} retained_bytes={}",
                elapsed.as_nanos(), before.0, after.0, after.1, symbols.frames.len(), symbols.bytes);
        }
    }
    #[test]
    fn file_symbolization_progresses_while_dynamic_loader_is_locked() {
        struct Probe {
            start: std::sync::mpsc::Sender<()>,
            done: std::sync::mpsc::Receiver<bool>,
            passed: bool,
        }
        unsafe extern "C" fn locked(
            _: *mut libc::dl_phdr_info,
            _: usize,
            data: *mut c_void,
        ) -> i32 {
            // glibc invokes this callback while holding its loader lock.
            // A timeout releases it, so a regression fails instead of
            // leaving the test runner deadlocked.
            let probe = unsafe { &mut *data.cast::<Probe>() };
            let _ = probe.start.send(());
            probe.passed = probe
                .done
                .recv_timeout(Duration::from_secs(15))
                .unwrap_or(false);
            1
        }
        // A release binary can have symbols but no DWARF. Require the same
        // information as an uncontended resolution, including file/line when
        // available; debug test builds must still resolve their source location.
        let pc = resolve_file as *const () as usize;
        let mut baseline = Symbols::default();
        baseline.refresh(&[pc]);
        let expected = baseline.frames.remove(&pc).unwrap();
        assert!(expected
            .iter()
            .any(|f| f.name.as_ref().is_some_and(|n| n.contains("resolve_file"))));
        if cfg!(debug_assertions) {
            assert!(expected.iter().any(|f| f
                .filename
                .as_ref()
                .is_some_and(|n| n.ends_with("profilesymbols.rs"))
                && f.line.is_some_and(|line| line > 0)));
        }
        drop(baseline);
        let (start, ready) = std::sync::mpsc::channel();
        let (finished, done) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            ready.recv().unwrap();
            let mut symbols = Symbols::default();
            symbols.refresh(&[pc]);
            let valid = symbols.frames.get(&pc) == Some(&expected);
            let old = symbols.frames.get(&pc).map(Vec::as_ptr);
            let mappings = symbols.mappings.clone();
            symbols.refresh(&[pc]);
            // Other tests may load GStreamer plugins concurrently. Reuse
            // cached frames only when the executable mappings stayed put.
            let _ = finished.send(
                valid
                    && (mappings != symbols.mappings
                        || old == symbols.frames.get(&pc).map(Vec::as_ptr)),
            );
        });
        let mut probe = Probe {
            start,
            done,
            passed: false,
        };
        // SAFETY: callback borrows probe only for the duration of this call.
        unsafe {
            libc::dl_iterate_phdr(Some(locked), (&mut probe as *mut Probe).cast());
        }
        worker.join().unwrap();
        assert!(
            probe.passed,
            "symbolization must preserve available symbols/source information without waiting for the loader lock"
        );
    }

    #[test]
    fn executable_map_parser_preserves_paths_and_offsets() {
        let map =
            Mapping::parse("00401000-00405000 r-xp 00001000 08:01 123 /a path/with  spaces.so")
                .unwrap();
        assert_eq!(
            (map.start, map.end, map.offset, map.inode),
            (0x401000, 0x405000, 0x1000, 123)
        );
        assert_eq!(map.path, PathBuf::from("/a path/with  spaces.so"));
        assert!(
            Mapping::parse("00401000-00405000 r-xp 00001000 08:01 123 /removed.so (deleted)")
                .is_none()
        );
        assert!(Mapping::parse("00401000-00405000 rw-p 00001000 08:01 123 /data.so").is_none());
    }
}
