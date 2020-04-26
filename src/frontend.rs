use std::slice;
use std::fs::File;
use std::io::{self, prelude::*};
use byteorder::{WriteBytesExt, LittleEndian};
use crate::interface::*;

extern "C" {
    pub fn trs_enable();
    pub fn trs_disable();
    pub fn trs_init(bufptr: *mut RetStack, len: usize);
    pub fn trs_get_events() -> *const Event;
    pub fn trs_get_events_index() -> usize;
}

pub fn enable() {
    unsafe{trs_enable()}
}

pub fn disable() {
    unsafe{trs_disable()}
}

pub fn get_events() -> (&'static [Event], usize) {
    let events = unsafe {
        slice::from_raw_parts(trs_get_events(), MAX_RECORDED_EVENTS)
    };
    let idx = unsafe { trs_get_events_index() };
    (events, idx)
}


pub fn init() {
    let buf = Vec::<RetStack>::with_capacity(10);
    unsafe {
        // intentionally leak here! stacks have to live until end of application.
        let (ptr, _len, cap) = buf.into_raw_parts();
        trs_init(ptr, cap)
    }
}


pub fn dump_full_uftrace(out_dir: &str, binary_name: &str) -> io::Result<()> {
    //! Dumps the traces with some faked metadata into out_dir. Can be directly parsed with uftrace.
    //! 
    //! Will NOT generate symbols! You can generate them with `nm -n $BINARY > binary_name.sym`
    //! 
    //! binary_name is only relevant for this symbol file. Generated metadata instructs uftrace where to look for it.
    //! 
    //! out_dir has to point to a folder, which has to exist.

    // arbitrary values for pid and sid
    let pid = 42;
    let sid = "00";

    // First lets create all traces.
    let tids = dump_traces(out_dir, false)?;


    println!("Creating fake uftrace data dir at {}..", out_dir);
    println!("  Creating ./info");
    let mut info: Vec<u8> = Vec::new();
    
    // /info HEADER
    // magic
    info.extend("Ftrace!\x00".as_bytes());
    // version. we are using version 4 of fileformat
    info.write_u32::<LittleEndian>(4).expect("Write interrupted");
    // header size. 0x28 == 40 bytes
    info.write_u16::<LittleEndian>(40).expect("Write interrupted");
    // endinaness = 1
    info.push(1);
    // elf_ident[EI_CLASS]. always 2 for 64bit
    info.push(2);
    // feature flags
    println!("    feats = TASK_SESSION");
    const TASK_SESSION: u64 = 1 << 1; // needed.
    info.write_u64::<LittleEndian>(TASK_SESSION).expect("Write interrupted");
    // info flags
    println!("    info = CMDLINE | TASKINFO");
    const CMDLINE: u64 = 1 << 3; // needed, else --dump chrome outputs invalid json.
    const TASKINFO: u64 = 1 << 7; // needed, since uftrace uses this to determine how to interpret task.txt
    info.write_u64::<LittleEndian>(CMDLINE | TASKINFO).expect("Write interrupted");
    // mstack. disable in feature flags, so 0
    info.write_u16::<LittleEndian>(0).expect("Write interrupted");
    // reserved
    info.write_u16::<LittleEndian>(0).expect("Write interrupted");
    info.write_u16::<LittleEndian>(0).expect("Write interrupted");
    info.write_u16::<LittleEndian>(0).expect("Write interrupted");
    // /info END OF HEADER
    
    // cmdline
    println!("    cmdline = 'fakeuftrace'");
    write!(info, "cmdline:fakeuftrace\n")?;
    // taskinfo
    println!("    tid = {:?}", tids);
    write!(info, "taskinfo:lines=2\n")?;
    write!(info, "taskinfo:nr_tid={}\n", tids.len())?;
    write!(info, "taskinfo:tids={}", tids[0])?;
    for tid in &tids[1..] {
        write!(info, ",{}", tid)?;
    }
    write!(info, "\n")?;

    let infofile = format!("{}/info", out_dir);
    let mut infofile = File::create(infofile)?;
    infofile.write_all(&info[..])?;
    drop(infofile);


    println!("  Creating ./task.txt");
    let taskfile = format!("{}/task.txt", out_dir);
    let mut taskfile = File::create(taskfile)?;
    println!("    pid = {}", pid);
    println!("    sid = {}", sid);
    println!("    exe = {}", binary_name);
    write!(taskfile, "SESS timestamp=0.0 pid={} sid={} exename=\"{}\"\n", pid, sid, binary_name)?;
    for tid in tids {
        write!(taskfile, "TASK timestamp=0.0 tid={} pid={}\n", tid, pid)?;
    }
    drop(taskfile);


    println!("  Creating ./sid-{}.map memory map file", sid);
    let mapfile = format!("{}/sid-{}.map", out_dir, sid);
    let mut mapfile = File::create(mapfile)?;
    write!(mapfile, "000000000000-ffffffffffff r-xp 00000000 00:00 0                          {}\n", binary_name)?;
    write!(mapfile, "ffffffffffff-ffffffffffff rw-p 00000000 00:00 0                          [stack]\n")?;

    println!("  You should generate symbols with `nm -n $BINARY > {}.sym`", binary_name);

    Ok(())
}


pub fn dump_trace(outfile: &str) -> io::Result<()> {
    dump_traces(outfile, true)?;
    Ok(())
}


fn dump_traces(outpath: &str, singlefile: bool) -> io::Result<Vec<u64>> {
    // Uftraces trace format: a bunch of 64-bit fields.
    // two 64bit for one event. See https://github.com/namhyung/uftrace/wiki/Data-Format
    // 
    /* struct uftrace_record {
        uint64_t time;
        uint64_t type:   2;
        uint64_t more:   1;
        uint64_t magic:  3;
        uint64_t depth:  10;
        uint64_t addr:   48; /* child ip or uftrace_event_id */
    }; */

    // TODO: create enable lock, to ensure no mcount() happens while we read the events array.
    disable();
    println!("Saving traces to disk...!");

    // To avoid to many reallocs, use array with maximum size for all traces.
    let mut out = Vec::<u8>::with_capacity(16*MAX_RECORDED_EVENTS);
    
    let (events, cidx) = get_events();
    let cidx = cidx % MAX_RECORDED_EVENTS;

    // Keep track of tids so we can assemble metadata
    let mut tids: Vec<u64> = Vec::new();

    let mut current_tid: Option<core::num::NonZeroU64> = None; // should never be None, but lets be sure.

    loop {
        // clear out vec incase it contains entries from previous tid
        out.clear();
        
        println!("  Parsing TID {:?}...!", current_tid);
        for e in events[cidx..].iter().chain(events[..cidx].iter()) {
            match e {
                Event::Exit(e) => {
                    if !singlefile && current_tid != e.tid {continue};
                    write_event(&mut out, e.time, e.from, 1);
                },
                Event::Entry(e) => {
                    if !singlefile &&  current_tid != e.tid {continue};
                    write_event(&mut out, e.time, e.to, 0);
                }
                Event::Empty => {
                    continue;
                }
            }
        }
        
        if !out.is_empty() {
            let filename = if singlefile {
                outpath.into()
            } else {
                let tid = current_tid.map_or(0, |tid| tid.get());
                tids.push(tid);
                let file = format!("{}.dat", tid);
                format!("{}/{}", outpath, file)
            };

            println!("  Writing to disk: {} events ({})", out.len(), filename);

            let mut file = File::create(filename)?;
            file.write_all(&out[..])?;
        } else if current_tid.is_some() {
            // we expect no events for current_tid == None. But if we see none for a higher tid, we assume we have parsed all events.
            println!("  Parsed all events!");
            break;
        }

        // Increase current_tid by 1 (or set to 1 if None)
        current_tid = current_tid.map_or(
            core::num::NonZeroU64::new(1),
            |tid| core::num::NonZeroU64::new(tid.get() + 1)
        );
    }
    Ok(tids)
}


fn write_event(out: &mut Vec<u8>, time: u64, addr: *const usize, kind: u64) {
    out.write_u64::<LittleEndian>(time).expect("Write interrupted");
    
    let mut merged: u64 = 0;
    merged |= (kind & 0b11) << 0; // type = UFTRACE_EXIT / UFTRACE_ENTRY
    merged |= 0 << 2; // more, always 0
    merged |= 0b101 << 3; // magic, always 0b101
    merged |= (0 & ((1<<10) - 1)) << 6; // depth
    merged |= (addr as u64 & ((1<<48)-1)) << 16; // actual address, limited to 48 bit.
    out.write_u64::<LittleEndian>(merged).expect("Write interrupted");
}
