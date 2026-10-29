use lattice_seams::{Clock, Io, OsRng, Rng, StdIo, SystemClock};
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_sql_planner_spike::{
    Error, Result,
    ast::{Plan, Value},
    custom, execute,
    storage::{Db, Session},
};
use serde_json::{Value as Json, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, Write},
    path::Path,
    time::Instant,
};

#[derive(Clone, Copy, Debug)]
enum Candidate {
    Custom,
    Parser,
    Fusion,
}
struct Driver {
    candidate: Candidate,
    #[cfg(feature = "datafusion")]
    sim: bool,
    cache: BTreeMap<String, Plan>,
    #[cfg(feature = "datafusion")]
    runtime: Option<tokio::runtime::Runtime>,
    #[cfg(feature = "datafusion")]
    fusion_cache: BTreeMap<String, lattice_sql_planner_spike::fusion::PreparedInsert>,
}
impl Driver {
    fn new(candidate: Candidate, sim: bool) -> Self {
        let _ = sim;
        Self {
            candidate,
            #[cfg(feature = "datafusion")]
            sim,
            cache: BTreeMap::new(),
            #[cfg(feature = "datafusion")]
            runtime: if sim {
                None
            } else {
                Some(
                    tokio::runtime::Builder::new_current_thread()
                        .enable_time()
                        .build()
                        .unwrap(),
                )
            },
            #[cfg(feature = "datafusion")]
            fusion_cache: BTreeMap::new(),
        }
    }
    fn parse(&self, sql: &str) -> Result<Plan> {
        match self.candidate {
            Candidate::Custom => custom::parse(sql),
            #[cfg(feature = "parser")]
            Candidate::Parser | Candidate::Fusion => lattice_sql_planner_spike::parsed::parse(sql),
            #[cfg(not(feature = "parser"))]
            _ => Err(Error::unsupported("candidate not compiled")),
        }
    }
    fn run<I: Io, C: Clock, R: Rng>(
        &mut self,
        db: &mut Db<I, C, R>,
        s: &mut Session,
        sql: &str,
        params: &[Value],
        prepared: bool,
    ) -> Result<execute::Output> {
        let plan = if prepared {
            if !self.cache.contains_key(sql) {
                let p = self.parse(sql)?;
                self.cache.insert(sql.into(), p);
            }
            self.cache[sql].clone()
        } else {
            self.parse(sql)?
        };
        if !matches!(self.candidate, Candidate::Fusion) {
            return execute::execute(db, s, &plan, params);
        }
        #[cfg(feature = "datafusion")]
        {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if prepared && matches!(plan, Plan::Insert { .. }) {
                    if !self.fusion_cache.contains_key(sql) {
                        let future =
                            lattice_sql_planner_spike::fusion::PreparedInsert::prepare(&plan);
                        let p = if let Some(rt) = &self.runtime {
                            rt.block_on(future)
                        } else {
                            futures::executor::block_on(future)
                        }?;
                        self.fusion_cache.insert(sql.into(), p);
                    }
                    let p = &self.fusion_cache[sql];
                    let future = p.execute(db, s, params);
                    if let Some(rt) = &self.runtime {
                        rt.block_on(future)
                    } else {
                        futures::executor::block_on(future)
                    }
                } else {
                    let future = lattice_sql_planner_spike::fusion::execute(db, s, &plan, params);
                    if let Some(rt) = &self.runtime {
                        rt.block_on(future)
                    } else {
                        futures::executor::block_on(future)
                    }
                }
            }));
            return result.unwrap_or_else(|p| {
                db.rollback(s); // A panicking candidate cannot leave a live mutation buffer behind.
                let detail = p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "non-string panic".into());
                Err(Error::new(
                    "XX000",
                    "SPIKE-DATAFUSION-PANIC",
                    format!("sim_no_runtime={}: {detail}", self.sim),
                ))
            });
        }
        #[cfg(not(feature = "datafusion"))]
        Err(Error::unsupported("DataFusion feature not compiled"))
    }
}
fn i(n: i64) -> Value {
    Value::Int(n)
}
fn scenario(candidate: Candidate, seed: u64) -> Json {
    let mut driver = Driver::new(candidate, true);
    let mut db = Db::open(
        SimIo::new(seed, FaultProfile::clean()),
        SimClock::new(1000),
        SeededRng::new(seed.wrapping_add(7)),
    )
    .unwrap();
    db.seed(20).unwrap();
    let history = db.receipts[0].changeset.clone();
    let mut session = Session::default();
    let mut cases = Vec::new();
    let mut run = |name: &str, sql: &str, args: Vec<Value>, prepared: bool| {
        let result = driver.run(&mut db, &mut session, sql, &args, prepared);
        cases.push(json!({"construct":name,"sql":sql,"params":args,"result":result,"receipts":db.receipts}));
    };
    run(
        "join_filter",
        "SELECT tickets.id, users.name FROM tickets INNER JOIN users ON tickets.owner = users.id WHERE tickets.priority >= $1 ORDER BY tickets.id LIMIT 5",
        vec![i(5)],
        false,
    );
    run(
        "group_having",
        "SELECT owner, COUNT(*) FROM tickets GROUP BY owner HAVING COUNT(*) >= $1 ORDER BY owner",
        vec![i(1)],
        false,
    );
    run(
        "update_returning",
        "UPDATE tickets SET priority = priority + 1 WHERE id = $1 RETURNING id, priority",
        vec![i(2)],
        false,
    );
    run(
        "as_of",
        &format!("SELECT id, priority FROM tickets AS OF CHANGESET '{history}' WHERE id = $1"),
        vec![i(2)],
        false,
    );
    run("begin", "BEGIN", vec![], false);
    let insert = "INSERT INTO tickets (id,owner,priority) VALUES ($1,$2,$3) RETURNING id, priority";
    run("prepared_insert", insert, vec![i(50), i(2), i(3)], true);
    run(
        "prepared_insert_rebind",
        insert,
        vec![i(51), i(3), i(4)],
        true,
    );
    run(
        "own_write",
        "SELECT id, priority FROM tickets WHERE id >= 50 ORDER BY id",
        vec![],
        false,
    );
    run("commit", "COMMIT", vec![], false);
    run("begin_rollback", "BEGIN", vec![], false);
    run(
        "rollback_write",
        "UPDATE tickets SET priority = 6 WHERE id = 2 RETURNING id,priority",
        vec![],
        false,
    );
    run("rollback", "ROLLBACK", vec![], false);
    for (sql, _, _) in lattice_sql_planner_spike::negatives() {
        run("negative", &sql, vec![], false);
    }
    drop(run);
    let fixture = lattice_sql_planner_spike::negatives();
    for case in cases.iter_mut().filter(|c| c["construct"] == "negative") {
        let expected = fixture
            .iter()
            .find(|(sql, _, _)| case["sql"] == sql.as_str())
            .unwrap();
        case["expected_error"] = json!({"state":expected.1,"code":expected.2});
        case["matched_error"] = json!(
            case["result"]["Err"]["state"] == expected.1
                && case["result"]["Err"]["code"] == expected.2
        );
    }
    let negatives_match = cases
        .iter()
        .filter(|c| c["construct"] == "negative")
        .all(|c| c["matched_error"] == true);
    let positive = cases
        .iter()
        .filter(|c| c["construct"] != "negative")
        .all(|c| c["result"].get("Ok").is_some());
    json!({"candidate":format!("{candidate:?}"),"mode":"simulator_no_runtime","positive_cases_complete":positive,"C6_negative_fixtures_match":negatives_match,"cases":cases,"trace":db.trace,"C2_random_hash_audit":"unverified; matching traces alone does not prove absence of randomized hashing"})
}
#[allow(clippy::disallowed_methods)] // Standalone host boundary: real filesystem and measurement clock.
fn serve(candidate: Candidate, root: &Path, rows: usize) {
    std::fs::create_dir_all(root).unwrap();
    let mut db = Db::open(StdIo::new(root), SystemClock, OsRng).unwrap();
    db.tracing = false;
    if db.receipts.is_empty() {
        db.seed(rows).unwrap();
    }
    let mut session = Session::default();
    let mut driver = Driver::new(candidate, false);
    let mut out = std::io::stdout();
    writeln!(out,"{}",json!({"ready":true,"rows":rows,"fault_hooks":cfg!(feature="fault-hooks"),"durability":"StdIo fsync per commit; no group commit; WAL only; no checkpoint","receipts":db.receipts.iter().map(|r|json!({"lsn":r.lsn,"changeset":r.changeset})).collect::<Vec<_>>()})).unwrap();
    out.flush().unwrap();
    for line in std::io::stdin().lock().lines() {
        let request: Json = serde_json::from_str(&line.unwrap()).unwrap();
        let sql = request["sql"].as_str().unwrap();
        let args: Vec<Value> =
            serde_json::from_value(request.get("params").cloned().unwrap_or(json!([]))).unwrap();
        #[cfg(feature = "fault-hooks")]
        if let Some(n) = request.get("commit_barrier").and_then(Json::as_u64) {
            db.before_commit = Some(Box::new(move |writes| {
                println!("{}", json!({"before_commit":n,"staged_rows":writes}));
                std::io::stdout().flush().unwrap();
                loop {
                    std::thread::park();
                }
            }));
        }
        let start = Instant::now();
        let result = driver.run(
            &mut db,
            &mut session,
            sql,
            &args,
            request["prepared"].as_bool().unwrap_or(false),
        );
        let elapsed = start.elapsed().as_nanos();
        writeln!(
            out,
            "{}",
            json!({"result":result,"engine_ns":elapsed,"lsn":db.rows.applied_lsn()})
        )
        .unwrap();
        out.flush().unwrap();
    }
}
#[allow(clippy::disallowed_methods)] // Standalone host report/config files, outside production engine graph.
fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let arg = |name: &str, default: &str| {
        args.windows(2)
            .find(|a| a[0] == name)
            .map(|a| a[1].clone())
            .unwrap_or_else(|| default.into())
    };
    let candidate = match arg("--candidate", "custom").as_str() {
        "custom" => Candidate::Custom,
        "parser" => Candidate::Parser,
        "datafusion" => Candidate::Fusion,
        other => panic!("unknown candidate {other}"),
    };
    let mode = arg("--mode", "sim");
    if mode == "serve" {
        serve(
            candidate,
            Path::new(&arg("--dir", "planner-host")),
            arg("--rows", "20000").parse().unwrap(),
        );
        return;
    }
    if mode == "verify-kill" {
        let root = arg("--dir", "");
        let observed: Json =
            serde_json::from_slice(&std::fs::read(arg("--observed", "")).unwrap()).unwrap();
        let report = verify_kill(
            Path::new(&root),
            arg("--rows", "2000").parse().unwrap(),
            &observed,
        );
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        return;
    }
    let report = if mode == "sim" {
        let seed = arg("--seed", "42").parse().unwrap();
        let a = scenario(candidate, seed);
        let b = scenario(candidate, seed);
        json!({"repeat_trace_equal":a["trace"]==b["trace"],"run":a,"C1_budget":"unresolved: ADR references a budget without specifying it","C1_effort_days":null,"ADR_status":"proposed"})
    } else {
        panic!("unknown mode {mode}")
    };
    let output = arg("--output", "-");
    if output == "-" {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        std::fs::write(output, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}

fn verify_kill(root: &Path, count: usize, observed: &Json) -> Json {
    let mut db = Db::open(StdIo::new(root), SystemClock, OsRng).unwrap();
    db.tracing = false;
    let submitted = observed["submitted"].as_u64().unwrap() as usize;
    let receipts = db.receipts.clone();
    assert!(!receipts.is_empty());
    assert!(receipts.len() - 1 <= submitted);
    let mut unique = std::collections::BTreeSet::new();
    for (index, receipt) in receipts.iter().enumerate() {
        assert!(unique.insert(&receipt.changeset));
        let mut expected = BTreeMap::new();
        for id in 0..count {
            let row = vec![
                i(id as i64),
                i((id % 100) as i64),
                i((id % 7) as i64 + index as i64),
            ];
            expected.insert(
                (1, (id as i64).to_be_bytes().to_vec()),
                Some(serde_json::to_vec(&row).unwrap()),
            );
        }
        if index == 0 {
            for id in 0..100i64 {
                let row = vec![i(id), Value::Text(format!("user{id:03}"))];
                expected.insert(
                    (2, id.to_be_bytes().to_vec()),
                    Some(serde_json::to_vec(&row).unwrap()),
                );
            }
        }
        assert_eq!(
            receipt.writes,
            expected.into_iter().collect::<Vec<_>>(),
            "partial/unknown statement envelope {index}"
        );
    }
    for ack in observed["acked"].as_array().unwrap() {
        let index = ack["n"].as_u64().unwrap() as usize;
        assert_eq!(
            receipts.get(index + 1).map(|r| r.lsn),
            ack["lsn"].as_u64(),
            "observed receipt missing"
        );
    }
    let final_priority = (receipts.len() - 1) as i64;
    let rows = db.scan(&Session::default(), "tickets", &None).unwrap();
    assert_eq!(rows.len(), count);
    for (id, row) in rows.iter().enumerate() {
        assert_eq!(
            *row,
            vec![
                i(id as i64),
                i((id % 100) as i64),
                i((id % 7) as i64 + final_priority)
            ]
        );
    }
    db.rows.check_indexes().unwrap();
    for priority in final_priority..final_priority + 8 {
        let expected = (0..count)
            .filter(|id| (*id % 7) as i64 + final_priority == priority)
            .map(|id| (id as i64).to_be_bytes().to_vec())
            .collect::<Vec<_>>();
        assert_eq!(
            db.rows
                .lookup_at(db.rows.snapshot(), 9, &priority.to_be_bytes()),
            expected
        );
    }
    assert!(
        lattice_core::scrub::scrub(&db.io, "wal")
            .unwrap()
            .issues
            .is_empty()
    );
    drop(db);
    let mut reopened = Db::open(StdIo::new(root), SystemClock, OsRng).unwrap();
    assert_eq!(
        reopened
            .scan(&Session::default(), "tickets", &None)
            .unwrap(),
        rows
    );
    reopened.rows.check_indexes().unwrap();
    json!({"complete":true,"submitted":submitted,"observed_acks":observed["acked"].as_array().unwrap().len(),"recovered_statements":receipts.len()-1,"terminal_reopen":true,"log_scrub":"clean","index_scrub":"clean","process_only":true})
}
