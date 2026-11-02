//! Opens one database from two processes at once, each taking its byte-range locks either through
//! std (`File::lock_range` and its relatives) or through redb's own platform calls, and checks
//! that every pairing of the two implementations admits and refuses exactly the same opens, and
//! that writers of both exclude each other. Run it with no arguments: it re-executes itself for
//! each participant.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

macro_rules! implementation {
    ($module:ident, $redb:ident) => {
        mod $module {
            use std::path::Path;
            use $redb::{
                ConcurrencyMode, Database, DatabaseError, ReadOnlyDatabase, ReadableDatabase,
                ReadableTable, TableDefinition,
            };

            const COUNTER: TableDefinition<u64, u64> = TableDefinition::new("counter");

            fn builder(mode: &str) -> $redb::Builder {
                let mode = match mode {
                    "exclusive" => ConcurrencyMode::ExclusiveWriter,
                    "single" => ConcurrencyMode::SingleWriter,
                    "multi" => ConcurrencyMode::MultiWriter,
                    _ => panic!("unknown concurrency mode {mode}"),
                };
                let mut builder = Database::builder();
                builder.set_concurrency_mode(mode);
                builder
            }

            fn outcome(err: DatabaseError) -> String {
                match err {
                    DatabaseError::DatabaseAlreadyOpen => "already-open".to_string(),
                    DatabaseError::LockProtocolMismatch => "protocol-mismatch".to_string(),
                    err => format!("error: {err}"),
                }
            }

            #[allow(dead_code)]
            pub enum Handle {
                Writable(Database),
                ReadOnly(ReadOnlyDatabase),
            }

            pub fn create(path: &Path) {
                let db = Database::create(path).unwrap();
                let txn = db.begin_write().unwrap();
                txn.open_table(COUNTER).unwrap();
                txn.commit().unwrap();
            }

            pub fn open(path: &Path, mode: &str, access: &str) -> Result<Handle, String> {
                let builder = builder(mode);
                match access {
                    "rw" => builder.open(path).map(Handle::Writable),
                    "ro" => builder.open_read_only(path).map(Handle::ReadOnly),
                    _ => panic!("unknown access {access}"),
                }
                .map_err(outcome)
            }

            /// Adds one to the counter `count` times, in a write transaction each
            pub fn increment(path: &Path, mode: &str, count: u64) {
                let db = builder(mode).open(path).unwrap();
                for _ in 0..count {
                    let txn = db.begin_write().unwrap();
                    {
                        let mut table = txn.open_table(COUNTER).unwrap();
                        let value = table.get(0).unwrap().map_or(0, |value| value.value());
                        table.insert(0, value + 1).unwrap();
                    }
                    txn.commit().unwrap();
                }
            }

            pub fn counter(path: &Path) -> u64 {
                let db = builder("exclusive").open_read_only(path).unwrap();
                let txn = db.begin_read().unwrap();
                let table = txn.open_table(COUNTER).unwrap();
                table.get(0).unwrap().map_or(0, |value| value.value())
            }
        }
    };
}

implementation!(own_locks, redb_own);
implementation!(std_locks, redb_std);

const IMPLEMENTATIONS: [&str; 2] = ["own", "std"];
const OPENS: [(&str, &str); 6] = [
    ("exclusive", "rw"),
    ("exclusive", "ro"),
    ("single", "rw"),
    ("single", "ro"),
    ("multi", "rw"),
    ("multi", "ro"),
];

macro_rules! dispatch {
    ($implementation:expr, $function:ident($($arg:expr),*)) => {
        match $implementation {
            "own" => own_locks::$function($($arg),*),
            "std" => std_locks::$function($($arg),*),
            other => panic!("unknown implementation {other}"),
        }
    };
}

/// One participant: reports its open on stdout, and with `hold`, keeps the database open until
/// stdin closes
fn participant(args: &[String]) {
    let [command, implementation, path, mode, access] = args else {
        panic!("usage: <hold|probe> <own|std> <path> <mode> <rw|ro>");
    };
    let path = Path::new(path);
    let opened = match implementation.as_str() {
        "own" => own_locks::open(path, mode, access)
            .map(|handle| Box::new(handle) as Box<dyn std::any::Any>),
        "std" => std_locks::open(path, mode, access)
            .map(|handle| Box::new(handle) as Box<dyn std::any::Any>),
        other => panic!("unknown implementation {other}"),
    };
    let report = match &opened {
        Ok(_) => "ok".to_string(),
        Err(outcome) => outcome.clone(),
    };
    println!("{report}");
    std::io::stdout().flush().unwrap();
    if command == "hold" && opened.is_ok() {
        let mut rest = Vec::new();
        std::io::stdin().read_to_end(&mut rest).unwrap();
    }
    drop(opened);
}

fn spawn(args: &[&str]) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

fn report(child: &mut Child) -> String {
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    line.trim().to_string()
}

/// What each open reports while another holds the database, one line per pair of opens
fn open_matrix(holder: &str, prober: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for (held_mode, held_access) in OPENS {
        for (probe_mode, probe_access) in OPENS {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("db.redb");
            dispatch!(holder, create(&path));
            let path = path.to_str().unwrap();

            let mut held = spawn(&["hold", holder, path, held_mode, held_access]);
            let held_report = report(&mut held);
            let probed = if held_report == "ok" {
                let mut probe = spawn(&["probe", prober, path, probe_mode, probe_access]);
                let probed = report(&mut probe);
                assert!(probe.wait().unwrap().success());
                probed
            } else {
                format!("holder: {held_report}")
            };
            drop(held.stdin.take());
            assert!(held.wait().unwrap().success());
            lines.push(format!(
                "{held_mode:>9} {held_access} held, {probe_mode:>9} {probe_access} -> {probed}"
            ));
        }
    }
    lines
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        if args[0] == "increment" {
            let [_, implementation, path, mode, count] = &args[..] else {
                panic!("usage: increment <own|std> <path> <mode> <count>");
            };
            dispatch!(
                implementation.as_str(),
                increment(Path::new(path), mode, count.parse().unwrap())
            );
        } else {
            participant(&args);
        }
        return;
    }

    let mut matrices = Vec::new();
    for holder in IMPLEMENTATIONS {
        for prober in IMPLEMENTATIONS {
            matrices.push(((holder, prober), open_matrix(holder, prober)));
        }
    }
    let (_, reference) = &matrices[0];
    for line in reference {
        println!("{line}");
    }
    for ((holder, prober), matrix) in &matrices {
        assert_eq!(
            matrix, reference,
            "{holder} holding against {prober} probing differs from own against own"
        );
        println!("{holder} holding, {prober} probing: identical");
    }
    // The locks are in force, not merely equally absent
    let expect = |line: &str| assert!(reference.iter().any(|l| l == line), "missing: {line}");
    expect("exclusive rw held, exclusive rw -> already-open");
    expect("exclusive rw held,     multi rw -> already-open");
    expect("exclusive ro held, exclusive ro -> ok");
    expect("    multi rw held,     multi rw -> ok");
    expect("   single rw held,    single rw -> already-open");
    expect("   single rw held,    single ro -> ok");

    // Writers of both implementations, at once, in a mode that lets them share the file: each
    // write transaction takes the writer byte with a blocking lock, so no increment is lost
    for mode in ["multi"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.redb");
        std_locks::create(&path);
        let path_str = path.to_str().unwrap();
        let count = 250;
        let writers: Vec<Child> = IMPLEMENTATIONS
            .iter()
            .flat_map(|implementation| {
                [0, 1].map(|_| {
                    spawn(&[
                        "increment",
                        implementation,
                        path_str,
                        mode,
                        &count.to_string(),
                    ])
                })
            })
            .collect();
        let writer_count = writers.len() as u64;
        for mut writer in writers {
            assert!(writer.wait().unwrap().success());
        }
        let total = own_locks::counter(&path);
        assert_eq!(total, std_locks::counter(&path));
        assert_eq!(total, count * writer_count, "{mode}: lost increments");
        println!(
            "{mode}: {writer_count} concurrent writers, 2 per implementation: {total} increments"
        );
    }
    println!("interop ok");
}
