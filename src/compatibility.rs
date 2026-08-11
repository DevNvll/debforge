use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::archive;
use crate::error::{AppError, Context, Result};
use crate::process;

const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_STRTAB: i64 = 5;
const DT_STRSZ: i64 = 10;
const DT_RPATH: i64 = 15;
const DT_RUNPATH: i64 = 29;
const MAX_PROGRAM_HEADERS: usize = 4096;
const MAX_DYNAMIC_ENTRIES: usize = 65_536;
const MAX_STRING_TABLE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompatibilityReport {
    pub elf_files: usize,
    pub foreign_elf_files: Vec<PathBuf>,
    pub privileged_files: Vec<PathBuf>,
    pub missing_interpreters: Vec<String>,
    pub missing_libraries: Vec<String>,
    pub risky_runtime_paths: Vec<String>,
}

impl CompatibilityReport {
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if !self.privileged_files.is_empty() {
            warnings.push(format!(
                "The payload contains {} setuid or setgid file(s); review their privileged modes before installation.",
                self.privileged_files.len()
            ));
        }
        if !self.foreign_elf_files.is_empty() {
            warnings.push(format!(
                "The payload contains {} bundled ELF file(s) for another architecture; host executables were checked separately.",
                self.foreign_elf_files.len()
            ));
        }
        if !self.missing_interpreters.is_empty() {
            warnings.push(format!(
                "ELF interpreters are not present on this system: {}.",
                self.missing_interpreters.join(", ")
            ));
        }
        if !self.missing_libraries.is_empty() {
            warnings.push(format!(
                "Shared libraries are not present before the Pacman transaction: {}.",
                self.missing_libraries.join(", ")
            ));
        }
        if !self.risky_runtime_paths.is_empty() {
            warnings.push(format!(
                "ELF runtime paths still use Debian multiarch locations: {}.",
                self.risky_runtime_paths.join(", ")
            ));
        }
        warnings
    }
}

#[derive(Debug, Clone)]
struct ParsedElf {
    class: ElfClass,
    little_endian: bool,
    machine: u16,
    interpreter: Option<String>,
    needed: Vec<String>,
    runtime_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElfClass {
    Elf32,
    Elf64,
}

#[derive(Debug, Clone, Copy)]
struct LoadSegment {
    offset: u64,
    virtual_address: u64,
    file_size: u64,
}

#[derive(Debug, Clone, Copy)]
struct ExpectedElf {
    class: ElfClass,
    little_endian: bool,
    machine: u16,
}

pub fn inspect_payload(root: &Path, architecture: &str) -> Result<CompatibilityReport> {
    let expected = expected_elf(architecture);
    let mut parsed = Vec::new();
    let mut report = CompatibilityReport::default();
    let mut provided_names = HashSet::new();

    for relative in archive::collect_sorted_file_list(root)? {
        let relative = relative.strip_prefix("./").unwrap_or(&relative);
        if relative.as_os_str().is_empty() {
            continue;
        }
        let path = root.join(relative);
        let metadata = fs::symlink_metadata(&path)
            .context(format!("Cannot inspect payload path {}", path.display()))?;
        let file_type = metadata.file_type();
        if file_type.is_block_device()
            || file_type.is_char_device()
            || file_type.is_fifo()
            || file_type.is_socket()
        {
            return Err(AppError::new(format!(
                "The Debian payload contains an unsupported special file: {}",
                relative.display()
            )));
        }
        if !file_type.is_file() {
            continue;
        }

        if metadata.mode() & 0o6000 != 0 {
            report.privileged_files.push(relative.to_path_buf());
        }
        if let Some(name) = relative.file_name().and_then(|value| value.to_str()) {
            provided_names.insert(name.to_string());
        }

        let Some(elf) = parse_elf(&path)? else {
            continue;
        };
        report.elf_files += 1;
        let architecture_matches = expected.is_some_and(|expected| {
            elf.class == expected.class
                && elf.little_endian == expected.little_endian
                && elf.machine == expected.machine
        });
        if !architecture_matches {
            if elf.interpreter.is_some() || is_command_path(relative) {
                return Err(AppError::new(format!(
                    "ELF architecture mismatch in executable {}: machine {}, class {:?}.",
                    relative.display(),
                    elf.machine,
                    elf.class
                )));
            }
            report.foreign_elf_files.push(relative.to_path_buf());
            continue;
        }
        parsed.push(elf);
    }

    let system_libraries = system_library_names();
    let mut missing_interpreters = BTreeSet::new();
    let mut missing_libraries = BTreeSet::new();
    let mut risky_runtime_paths = BTreeSet::new();
    for elf in parsed {
        if let Some(interpreter) = elf.interpreter {
            let payload_interpreter = interpreter
                .strip_prefix('/')
                .map(|value| root.join(value))
                .is_some_and(|value| value.exists());
            if !payload_interpreter && !Path::new(&interpreter).exists() {
                missing_interpreters.insert(interpreter);
            }
        }
        for library in elf.needed {
            if !provided_names.contains(&library) && !system_libraries.contains(&library) {
                missing_libraries.insert(library);
            }
        }
        for runtime_path in elf.runtime_paths {
            for entry in runtime_path.split(':') {
                if entry.contains("linux-gnu") {
                    risky_runtime_paths.insert(entry.to_string());
                }
            }
        }
    }
    report.missing_interpreters = missing_interpreters.into_iter().collect();
    report.missing_libraries = missing_libraries.into_iter().collect();
    report.risky_runtime_paths = risky_runtime_paths.into_iter().collect();
    Ok(report)
}

fn is_command_path(path: &Path) -> bool {
    path.starts_with("usr/bin") || path.starts_with("bin") || path.starts_with("usr/sbin")
}

fn parse_elf(path: &Path) -> Result<Option<ParsedElf>> {
    let mut file =
        File::open(path).context(format!("Cannot open ELF candidate {}", path.display()))?;
    let file_size = file
        .metadata()
        .context(format!("Cannot inspect ELF candidate {}", path.display()))?
        .len();
    let mut header = [0_u8; 64];
    let read = file
        .read(&mut header)
        .context(format!("Cannot read ELF candidate {}", path.display()))?;
    if read < ELF_MAGIC.len() || &header[..ELF_MAGIC.len()] != ELF_MAGIC {
        return Ok(None);
    }
    if read < 52 {
        return Err(AppError::new(format!(
            "Truncated ELF header in {}",
            path.display()
        )));
    }

    let class = match header[4] {
        1 => ElfClass::Elf32,
        2 => ElfClass::Elf64,
        value => {
            return Err(AppError::new(format!(
                "Unsupported ELF class {value} in {}",
                path.display()
            )));
        }
    };
    let little_endian = match header[5] {
        1 => true,
        2 => false,
        value => {
            return Err(AppError::new(format!(
                "Unsupported ELF byte order {value} in {}",
                path.display()
            )));
        }
    };
    let machine = read_u16(&header, 18, little_endian)?;
    let (program_offset, program_entry_size, program_count) = match class {
        ElfClass::Elf32 => (
            u64::from(read_u32(&header, 28, little_endian)?),
            usize::from(read_u16(&header, 42, little_endian)?),
            usize::from(read_u16(&header, 44, little_endian)?),
        ),
        ElfClass::Elf64 => {
            if read < 64 {
                return Err(AppError::new(format!(
                    "Truncated 64-bit ELF header in {}",
                    path.display()
                )));
            }
            (
                read_u64(&header, 32, little_endian)?,
                usize::from(read_u16(&header, 54, little_endian)?),
                usize::from(read_u16(&header, 56, little_endian)?),
            )
        }
    };
    let minimum_entry_size = match class {
        ElfClass::Elf32 => 32,
        ElfClass::Elf64 => 56,
    };
    if program_count > MAX_PROGRAM_HEADERS
        || (program_count > 0 && program_entry_size < minimum_entry_size)
    {
        return Err(AppError::new(format!(
            "Invalid ELF program-header table in {}",
            path.display()
        )));
    }
    let table_size = program_entry_size
        .checked_mul(program_count)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| AppError::new("ELF program-header table is too large."))?;
    if program_offset
        .checked_add(table_size)
        .is_none_or(|end| end > file_size)
    {
        return Err(AppError::new(format!(
            "ELF program-header table is outside {}",
            path.display()
        )));
    }

    let mut loads = Vec::new();
    let mut dynamic = None;
    let mut interpreter = None;
    for index in 0..program_count {
        let offset = program_offset + u64::try_from(index * program_entry_size).unwrap_or(u64::MAX);
        let entry = read_at(&mut file, offset, program_entry_size)?;
        let kind = read_u32(&entry, 0, little_endian)?;
        let (segment_offset, virtual_address, file_bytes) = match class {
            ElfClass::Elf32 => (
                u64::from(read_u32(&entry, 4, little_endian)?),
                u64::from(read_u32(&entry, 8, little_endian)?),
                u64::from(read_u32(&entry, 16, little_endian)?),
            ),
            ElfClass::Elf64 => (
                read_u64(&entry, 8, little_endian)?,
                read_u64(&entry, 16, little_endian)?,
                read_u64(&entry, 32, little_endian)?,
            ),
        };
        validate_file_range(segment_offset, file_bytes, file_size, path)?;
        match kind {
            PT_LOAD => loads.push(LoadSegment {
                offset: segment_offset,
                virtual_address,
                file_size: file_bytes,
            }),
            PT_DYNAMIC => dynamic = Some((segment_offset, file_bytes)),
            PT_INTERP => {
                if file_bytes == 0 || file_bytes > 4096 {
                    return Err(AppError::new(format!(
                        "Invalid ELF interpreter path in {}",
                        path.display()
                    )));
                }
                let bytes = read_at(
                    &mut file,
                    segment_offset,
                    usize::try_from(file_bytes).map_err(|_| {
                        AppError::new("ELF interpreter path is too large for this system.")
                    })?,
                )?;
                interpreter = Some(read_c_string(&bytes, 0)?);
            }
            _ => {}
        }
    }

    let (needed, runtime_paths) = match dynamic {
        Some((offset, size)) => parse_dynamic(
            &mut file,
            class,
            little_endian,
            offset,
            size,
            &loads,
            file_size,
        )?,
        None => (Vec::new(), Vec::new()),
    };
    Ok(Some(ParsedElf {
        class,
        little_endian,
        machine,
        interpreter,
        needed,
        runtime_paths,
    }))
}

fn parse_dynamic(
    file: &mut File,
    class: ElfClass,
    little_endian: bool,
    offset: u64,
    size: u64,
    loads: &[LoadSegment],
    file_size: u64,
) -> Result<(Vec<String>, Vec<String>)> {
    let entry_size = match class {
        ElfClass::Elf32 => 8_u64,
        ElfClass::Elf64 => 16_u64,
    };
    let entry_count = usize::try_from(size / entry_size)
        .unwrap_or(usize::MAX)
        .min(MAX_DYNAMIC_ENTRIES);
    let mut needed_offsets = Vec::new();
    let mut path_offsets = Vec::new();
    let mut string_address = None;
    let mut string_size = None;
    for index in 0..entry_count {
        let entry_offset = offset
            .checked_add(u64::try_from(index).unwrap_or(u64::MAX) * entry_size)
            .ok_or_else(|| AppError::new("ELF dynamic table offset overflowed."))?;
        let entry = read_at(file, entry_offset, entry_size as usize)?;
        let (tag, value) = match class {
            ElfClass::Elf32 => (
                i64::from(i32::from_ne_bytes(endian_array_4(
                    &entry,
                    0,
                    little_endian,
                )?)),
                u64::from(read_u32(&entry, 4, little_endian)?),
            ),
            ElfClass::Elf64 => (
                i64::from_ne_bytes(endian_array_8(&entry, 0, little_endian)?),
                read_u64(&entry, 8, little_endian)?,
            ),
        };
        match tag {
            DT_NULL => break,
            DT_NEEDED => needed_offsets.push(value),
            DT_STRTAB => string_address = Some(value),
            DT_STRSZ => string_size = Some(value),
            DT_RPATH | DT_RUNPATH => path_offsets.push(value),
            _ => {}
        }
    }

    if needed_offsets.is_empty() && path_offsets.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let address = string_address
        .ok_or_else(|| AppError::new("ELF dynamic strings have no string-table address."))?;
    let size = string_size
        .ok_or_else(|| AppError::new("ELF dynamic strings have no string-table size."))?;
    if size == 0 || size > MAX_STRING_TABLE_BYTES {
        return Err(AppError::new(
            "ELF dynamic string table has an invalid size.",
        ));
    }
    let string_offset = virtual_to_file_offset(address, loads)
        .ok_or_else(|| AppError::new("ELF dynamic string table is not in a load segment."))?;
    validate_file_range(string_offset, size, file_size, Path::new("ELF file"))?;
    let strings = read_at(
        file,
        string_offset,
        usize::try_from(size)
            .map_err(|_| AppError::new("ELF string table is too large for this system."))?,
    )?;
    let needed = needed_offsets
        .into_iter()
        .map(|value| read_c_string(&strings, value))
        .collect::<Result<Vec<_>>>()?;
    let runtime_paths = path_offsets
        .into_iter()
        .map(|value| read_c_string(&strings, value))
        .collect::<Result<Vec<_>>>()?;
    Ok((needed, runtime_paths))
}

fn virtual_to_file_offset(address: u64, loads: &[LoadSegment]) -> Option<u64> {
    loads.iter().find_map(|segment| {
        let difference = address.checked_sub(segment.virtual_address)?;
        (difference < segment.file_size)
            .then(|| segment.offset.checked_add(difference))
            .flatten()
    })
}

fn system_library_names() -> HashSet<String> {
    let mut libraries = HashSet::new();
    if let Some(ldconfig) = process::find_tool("ldconfig") {
        if let Ok(output) =
            process::capture_text(Command::new(ldconfig).arg("-p").stdin(Stdio::null()))
        {
            for line in output.lines() {
                if let Some(name) = line.split_whitespace().next() {
                    if name.contains(".so") {
                        libraries.insert(name.to_string());
                    }
                }
            }
        }
    }
    for directory in ["/usr/lib", "/usr/lib32"] {
        if let Ok(entries) = fs::read_dir(directory) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.contains(".so") {
                        libraries.insert(name.to_string());
                    }
                }
            }
        }
    }
    libraries
}

fn expected_elf(architecture: &str) -> Option<ExpectedElf> {
    match architecture {
        "x86_64" => Some(ExpectedElf {
            class: ElfClass::Elf64,
            little_endian: true,
            machine: 62,
        }),
        "i686" => Some(ExpectedElf {
            class: ElfClass::Elf32,
            little_endian: true,
            machine: 3,
        }),
        "aarch64" => Some(ExpectedElf {
            class: ElfClass::Elf64,
            little_endian: true,
            machine: 183,
        }),
        "arm" | "armv7h" => Some(ExpectedElf {
            class: ElfClass::Elf32,
            little_endian: true,
            machine: 40,
        }),
        "ppc64le" => Some(ExpectedElf {
            class: ElfClass::Elf64,
            little_endian: true,
            machine: 21,
        }),
        "ppc64" => Some(ExpectedElf {
            class: ElfClass::Elf64,
            little_endian: false,
            machine: 21,
        }),
        "riscv64" => Some(ExpectedElf {
            class: ElfClass::Elf64,
            little_endian: true,
            machine: 243,
        }),
        "s390x" => Some(ExpectedElf {
            class: ElfClass::Elf64,
            little_endian: false,
            machine: 22,
        }),
        _ => None,
    }
}

fn read_at(file: &mut File, offset: u64, size: usize) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))
        .context("Cannot seek in ELF file")?;
    let mut bytes = vec![0_u8; size];
    file.read_exact(&mut bytes)
        .context("Cannot read ELF file data")?;
    Ok(bytes)
}

fn validate_file_range(offset: u64, size: u64, file_size: u64, path: &Path) -> Result<()> {
    if offset.checked_add(size).is_none_or(|end| end > file_size) {
        Err(AppError::new(format!(
            "ELF segment is outside {}",
            path.display()
        )))
    } else {
        Ok(())
    }
}

fn read_c_string(strings: &[u8], offset: u64) -> Result<String> {
    let offset = usize::try_from(offset)
        .map_err(|_| AppError::new("ELF string offset is too large for this system."))?;
    let tail = strings
        .get(offset..)
        .ok_or_else(|| AppError::new("ELF string offset is outside its string table."))?;
    let end = tail
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(tail.len());
    if end == 0 {
        return Err(AppError::new("ELF dynamic string is empty."));
    }
    String::from_utf8(tail[..end].to_vec())
        .map_err(|_| AppError::new("ELF dynamic string is not valid UTF-8."))
}

fn read_u16(bytes: &[u8], offset: usize, little: bool) -> Result<u16> {
    let array = bytes
        .get(offset..offset + 2)
        .and_then(|value| <[u8; 2]>::try_from(value).ok())
        .ok_or_else(|| AppError::new("ELF integer is outside the available data."))?;
    Ok(if little {
        u16::from_le_bytes(array)
    } else {
        u16::from_be_bytes(array)
    })
}

fn read_u32(bytes: &[u8], offset: usize, little: bool) -> Result<u32> {
    let array = bytes
        .get(offset..offset + 4)
        .and_then(|value| <[u8; 4]>::try_from(value).ok())
        .ok_or_else(|| AppError::new("ELF integer is outside the available data."))?;
    Ok(if little {
        u32::from_le_bytes(array)
    } else {
        u32::from_be_bytes(array)
    })
}

fn read_u64(bytes: &[u8], offset: usize, little: bool) -> Result<u64> {
    let array = bytes
        .get(offset..offset + 8)
        .and_then(|value| <[u8; 8]>::try_from(value).ok())
        .ok_or_else(|| AppError::new("ELF integer is outside the available data."))?;
    Ok(if little {
        u64::from_le_bytes(array)
    } else {
        u64::from_be_bytes(array)
    })
}

fn endian_array_4(bytes: &[u8], offset: usize, little: bool) -> Result<[u8; 4]> {
    let mut array = bytes
        .get(offset..offset + 4)
        .and_then(|value| <[u8; 4]>::try_from(value).ok())
        .ok_or_else(|| AppError::new("ELF integer is outside the available data."))?;
    if little != cfg!(target_endian = "little") {
        array.reverse();
    }
    Ok(array)
}

fn endian_array_8(bytes: &[u8], offset: usize, little: bool) -> Result<[u8; 8]> {
    let mut array = bytes
        .get(offset..offset + 8)
        .and_then(|value| <[u8; 8]>::try_from(value).ok())
        .ok_or_else(|| AppError::new("ELF integer is outside the available data."))?;
    if little != cfg!(target_endian = "little") {
        array.reverse();
    }
    Ok(array)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::net::UnixListener;

    use super::{ElfClass, inspect_payload, parse_elf};

    fn test_root(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "debforge-compatibility-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("test root");
        path
    }

    fn minimal_x86_64_elf() -> Vec<u8> {
        let mut bytes = vec![0_u8; 64];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&2_u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
        bytes
    }

    #[test]
    fn reads_a_minimal_elf_header_and_rejects_wrong_architecture() {
        let root = test_root("architecture");
        let path = root.join("usr/bin/program");
        fs::create_dir_all(path.parent().expect("parent")).expect("bin");
        fs::write(&path, minimal_x86_64_elf()).expect("ELF");
        let elf = parse_elf(&path).expect("parse").expect("ELF");
        assert_eq!(elf.class, ElfClass::Elf64);
        assert_eq!(elf.machine, 62);
        assert!(inspect_payload(&root, "x86_64").is_ok());
        assert!(inspect_payload(&root, "aarch64").is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn rejects_special_payload_files() {
        let root = test_root("special");
        let socket = root.join("control.sock");
        let _listener = match UnixListener::bind(&socket) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                fs::remove_dir_all(root).expect("cleanup");
                return;
            }
            Err(error) => panic!("socket: {error}"),
        };
        let error = inspect_payload(&root, "x86_64").expect_err("special file");
        assert!(error.to_string().contains("special file"));
        fs::remove_dir_all(root).expect("cleanup");
    }
}
