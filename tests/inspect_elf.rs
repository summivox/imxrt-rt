//! Automatically inspect the programs generated by the examples.
//!
//! Do not refer to this as a specification for the runtime. These values
//! are subject to change.

#![allow(clippy::unusual_byte_groupings)] // Spacing delimits ITCM / DTCM / OCRAM banks.

use goblin::elf::Elf;
use std::{fs, path::PathBuf, process::Command};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Build an example, returning a path to the ELF.
fn cargo_build(board: &str) -> Result<PathBuf> {
    Command::new("cargo")
        .arg("build")
        .arg("--example=blink-rtic")
        .arg(format!("--features=board/{},board/rtic", board))
        .arg("--target=thumbv7em-none-eabihf")
        .arg(format!("--target-dir=target/{}", board))
        .arg("--quiet")
        .spawn()?
        .wait()?;

    let path = PathBuf::from(format!(
        "target/{}/thumbv7em-none-eabihf/debug/examples/blink-rtic",
        board
    ));
    Ok(path)
}

struct ImxrtBinary<'a> {
    elf: &'a Elf<'a>,
}

impl<'a> ImxrtBinary<'a> {
    fn new(elf: &'a Elf<'a>) -> Self {
        Self { elf }
    }

    fn symbol(&self, symbol_name: &str) -> Option<goblin::elf::Sym> {
        self.elf
            .syms
            .iter()
            .flat_map(|sym| self.elf.strtab.get_at(sym.st_name).map(|name| (sym, name)))
            .find(|(_, name)| symbol_name == *name)
            .map(|(sym, _)| sym)
    }

    fn fcb(&self) -> Result<Fcb> {
        self.symbol("FLEXSPI_CONFIGURATION_BLOCK")
            .map(|sym| Fcb {
                address: sym.st_value,
                size: sym.st_size,
            })
            .ok_or_else(|| {
                String::from("Could not find FLEXSPI_CONFIGURATION_BLOCK in program").into()
            })
    }

    fn flexram_config(&self) -> Result<u64> {
        self.symbol("__flexram_config")
            .map(|sym| sym.st_value)
            .ok_or_else(|| String::from("Could not find FlexRAM configuration in program").into())
    }

    fn section(&self, section_name: &str) -> Result<Section> {
        self.elf
            .section_headers
            .iter()
            .flat_map(|sec| {
                self.elf
                    .shdr_strtab
                    .get_at(sec.sh_name)
                    .map(|name| (sec, name))
            })
            .find(|(_, name)| section_name == *name)
            .map(|(sec, _)| Section {
                address: sec.sh_addr,
                size: sec.sh_size,
            })
            .ok_or_else(|| format!("Could not find {section_name} in program").into())
    }

    fn section_lma(&self, section: &Section) -> u64 {
        self.elf
            .program_headers
            .iter()
            .filter(|phdr| goblin::elf::program_header::PT_LOAD == phdr.p_type)
            .find(|phdr| {
                phdr.p_vaddr <= section.address && (phdr.p_vaddr + phdr.p_memsz) > section.address
            })
            .map(|phdr| section.address - phdr.p_vaddr + phdr.p_paddr)
            .unwrap_or(section.address) // VMA == LMA
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fcb {
    address: u64,
    size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Section {
    address: u64,
    size: u64,
}

const DTCM: u64 = 0x2000_0000;
const ITCM: u64 = 0x0000_0000;

const fn aligned(value: u64, alignment: u64) -> u64 {
    (value + (alignment - 1)) & !(alignment - 1)
}

#[test]
#[ignore = "building an example can take time"]
fn imxrt1010evk() {
    let path = cargo_build("imxrt1010evk").expect("Unable to build example");
    let contents = fs::read(path).expect("Could not read ELF file");
    let elf = Elf::parse(&contents).expect("Could not parse ELF");

    let binary = ImxrtBinary::new(&elf);
    assert_eq!(
        Fcb {
            address: 0x6000_0400,
            size: 512
        },
        binary.fcb().unwrap()
    );
    assert_eq!(binary.flexram_config().unwrap(), 0b11_10_0101);

    let stack = binary.section(".stack").unwrap();
    assert_eq!(
        Section {
            address: DTCM,
            size: 8 * 1024
        },
        stack,
        "stack not at ORIGIN(DTCM), or not 8 KiB large"
    );
    assert_eq!(binary.section_lma(&stack), stack.address);

    let vector_table = binary.section(".vector_table").unwrap();
    assert_eq!(
        Section {
            address: stack.address + stack.size,
            size: 16 * 4 + 240 * 4
        },
        vector_table,
        "vector table not at expected VMA behind the stack"
    );
    assert!(
        vector_table.address % 1024 == 0,
        "vector table is not 1024-byte aligned"
    );
    assert_eq!(binary.section_lma(&vector_table), 0x6000_2000);

    let text = binary.section(".text").unwrap();
    assert_eq!(text.address, ITCM, "text");
    assert_eq!(
        binary.section_lma(&text),
        0x6000_2000 + vector_table.size,
        "text VMA expected behind vector table"
    );

    let rodata = binary.section(".rodata").unwrap();
    assert_eq!(
        rodata.address,
        0x6000_2000 + vector_table.size + aligned(text.size, 16),
        "rodata LMA & VMA expected behind text"
    );
    assert_eq!(rodata.address, binary.section_lma(&rodata));

    let data = binary.section(".data").unwrap();
    assert_eq!(data.address, 0x2020_0000, "data VMA in OCRAM");
    assert_eq!(
        data.size, 4,
        "blink-rtic expected to have a single static mut u32"
    );
    assert_eq!(
        binary.section_lma(&data),
        rodata.address + aligned(rodata.size, 4),
        "data LMA starts behind rodata"
    );

    let bss = binary.section(".bss").unwrap();
    assert_eq!(
        bss.address,
        data.address + aligned(data.size, 4),
        "bss in OCRAM behind data"
    );
    assert_eq!(binary.section_lma(&bss), bss.address, "bss is NOLOAD");

    let uninit = binary.section(".uninit").unwrap();
    assert_eq!(
        uninit.address,
        bss.address + aligned(bss.size, 4),
        "uninit in OCRAM behind bss"
    );
    assert_eq!(
        binary.section_lma(&uninit),
        uninit.address,
        "uninit is NOLOAD"
    );

    let heap = binary.section(".heap").unwrap();
    assert_eq!(
        Section {
            address: vector_table.address + vector_table.size,
            size: 1024
        },
        heap,
        "1 KiB heap in DTCM behind vector table"
    );
    assert_eq!(heap.size, 1024);
    assert_eq!(binary.section_lma(&heap), heap.address, "Heap is NOLOAD");
}

#[test]
#[ignore = "building an example can take time"]
fn teensy4() {
    let path = cargo_build("teensy4").expect("Unable to build example");
    let contents = fs::read(path).expect("Could not read ELF file");
    let elf = Elf::parse(&contents).expect("Could not parse ELF");

    let binary = ImxrtBinary::new(&elf);
    assert_eq!(
        Fcb {
            address: 0x6000_0000,
            size: 512
        },
        binary.fcb().unwrap()
    );
    assert_eq!(
        binary.flexram_config().unwrap(),
        0b11111111_101010101010101010101010
    );

    let stack = binary.section(".stack").unwrap();
    assert_eq!(
        Section {
            address: DTCM,
            size: 8 * 1024
        },
        stack,
        "stack not at ORIGIN(DTCM), or not 8 KiB large"
    );
    assert_eq!(binary.section_lma(&stack), stack.address);

    let vector_table = binary.section(".vector_table").unwrap();
    assert_eq!(
        Section {
            address: stack.address + stack.size,
            size: 16 * 4 + 240 * 4
        },
        vector_table,
        "vector table not at expected VMA behind the stack"
    );
    assert!(
        vector_table.address % 1024 == 0,
        "vector table is not 1024-byte aligned"
    );
    assert_eq!(binary.section_lma(&vector_table), 0x6000_2000);

    let text = binary.section(".text").unwrap();
    assert_eq!(
        text.address,
        binary.section_lma(&vector_table) + vector_table.size,
        "text"
    );
    assert_eq!(
        binary.section_lma(&text),
        0x6000_2000 + vector_table.size,
        "text VMA expected behind vector table"
    );

    let rodata = binary.section(".rodata").unwrap();
    assert_eq!(
        rodata.address,
        vector_table.address + vector_table.size,
        "rodata LMA & VMA expected behind text"
    );
    assert_eq!(
        binary.section_lma(&rodata),
        binary.section_lma(&text) + aligned(text.size, 16)
    );

    let data = binary.section(".data").unwrap();
    assert_eq!(
        data.address,
        rodata.address + rodata.size,
        "data VMA in DTCM behind rodata"
    );
    assert_eq!(
        data.size, 4,
        "blink-rtic expected to have a single static mut u32"
    );
    assert_eq!(
        binary.section_lma(&data),
        binary.section_lma(&rodata) + aligned(rodata.size, 4),
        "data LMA starts behind rodata"
    );

    let bss = binary.section(".bss").unwrap();
    assert_eq!(
        bss.address,
        data.address + aligned(data.size, 4),
        "bss in DTCM behind data"
    );
    assert_eq!(binary.section_lma(&bss), bss.address, "bss is NOLOAD");

    let uninit = binary.section(".uninit").unwrap();
    assert_eq!(
        uninit.address,
        bss.address + aligned(bss.size, 4),
        "uninit in DTCM behind bss"
    );
    assert_eq!(
        binary.section_lma(&uninit),
        uninit.address,
        "uninit is NOLOAD"
    );

    let heap = binary.section(".heap").unwrap();
    assert_eq!(
        Section {
            address: uninit.address + aligned(uninit.size, 4),
            size: 1024
        },
        heap,
        "1 KiB heap in DTCM behind uninit"
    );
    assert_eq!(binary.section_lma(&heap), heap.address, "Heap is NOLOAD");
}

#[test]
#[ignore = "building an example can take time"]
fn imxrt1170evk_cm7() {
    let path = cargo_build("imxrt1170evk-cm7").expect("Unable to build example");
    let contents = fs::read(path).expect("Could not read ELF file");
    let elf = Elf::parse(&contents).expect("Could not parse ELF");

    let binary = ImxrtBinary::new(&elf);
    assert_eq!(
        Fcb {
            address: 0x3000_0400,
            size: 512
        },
        binary.fcb().unwrap()
    );
    assert_eq!(
        binary.flexram_config().unwrap(),
        0b1111111111111111_1010101010101010
    );

    let stack = binary.section(".stack").unwrap();
    assert_eq!(
        Section {
            address: DTCM,
            size: 8 * 1024
        },
        stack,
        "stack not at ORIGIN(DTCM), or not 8 KiB large"
    );
    assert_eq!(binary.section_lma(&stack), stack.address);

    let vector_table = binary.section(".vector_table").unwrap();
    assert_eq!(
        Section {
            address: stack.address + stack.size,
            size: 16 * 4 + 240 * 4
        },
        vector_table,
        "vector table not at expected VMA behind the stack"
    );
    assert!(
        vector_table.address % 1024 == 0,
        "vector table is not 1024-byte aligned"
    );
    assert_eq!(binary.section_lma(&vector_table), 0x3000_2000);

    let text = binary.section(".text").unwrap();
    assert_eq!(text.address, ITCM, "text");
    assert_eq!(
        binary.section_lma(&text),
        0x3000_2000 + vector_table.size,
        "text VMA expected behind vector table"
    );

    let rodata = binary.section(".rodata").unwrap();
    assert_eq!(
        rodata.address,
        vector_table.address + vector_table.size,
        "rodata moved to DTCM behind vector table"
    );
    assert_eq!(
        binary.section_lma(&rodata),
        0x3000_2000 + vector_table.size + aligned(text.size, 16),
    );

    let data = binary.section(".data").unwrap();
    assert_eq!(data.address, 0x2024_0000, "data VMA in OCRAM");
    assert_eq!(
        data.size, 4,
        "blink-rtic expected to have a single static mut u32"
    );
    assert_eq!(
        binary.section_lma(&data),
        binary.section_lma(&rodata) + aligned(rodata.size, 4),
        "data LMA starts behind rodata"
    );

    let bss = binary.section(".bss").unwrap();
    assert_eq!(
        bss.address,
        data.address + aligned(data.size, 4),
        "bss in OCRAM behind data"
    );
    assert_eq!(binary.section_lma(&bss), bss.address, "bss is NOLOAD");

    let uninit = binary.section(".uninit").unwrap();
    assert_eq!(
        uninit.address,
        bss.address + aligned(bss.size, 4),
        "uninit in OCRAM behind bss"
    );
    assert_eq!(
        binary.section_lma(&uninit),
        uninit.address,
        "uninit is NOLOAD"
    );

    let heap = binary.section(".heap").unwrap();
    assert_eq!(
        Section {
            address: rodata.address + aligned(rodata.size, 4),
            size: 0,
        },
        heap,
        "0 byte heap in DTCM behind rodata table"
    );
    assert_eq!(binary.section_lma(&heap), heap.address, "Heap is NOLOAD");
}
