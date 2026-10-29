use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_sql_planner_spike::{
    ast::Value,
    custom,
    execute::execute,
    storage::{Db, Session},
};
type Parse = fn(&str) -> lattice_sql_planner_spike::Result<lattice_sql_planner_spike::ast::Plan>;
fn run(parse: Parse) -> Vec<serde_json::Value> {
    let mut db = Db::open(
        SimIo::new(1, FaultProfile::clean()),
        SimClock::new(1000),
        SeededRng::new(2),
    )
    .unwrap();
    db.seed(12).unwrap();
    let baseline = db.receipts.last().unwrap().changeset.clone();
    let mut s = Session::default();
    let mut query =
        |sql: &str, args: &[Value]| execute(&mut db, &mut s, &parse(sql).unwrap(), args).unwrap();
    let join = query(
        "SELECT tickets.id, users.name FROM tickets INNER JOIN users ON tickets.owner = users.id WHERE tickets.priority >= $1 ORDER BY tickets.id LIMIT 3",
        &[Value::Int(5)],
    );
    assert_eq!(
        join,
        vec![
            vec![Value::Int(5), Value::Text("user005".into())],
            vec![Value::Int(6), Value::Text("user006".into())]
        ]
    );
    let groups = query(
        "SELECT owner, COUNT(*) FROM tickets GROUP BY owner HAVING COUNT(*) >= $1 ORDER BY owner",
        &[Value::Int(1)],
    );
    assert_eq!(groups.len(), 12);
    query("BEGIN", &[]);
    let inserted = parse(
        "INSERT INTO tickets (id, owner, priority) VALUES ($1, $2, $3) RETURNING id, priority",
    )
    .unwrap();
    assert_eq!(
        execute(
            &mut db,
            &mut s,
            &inserted,
            &[Value::Int(50), Value::Int(2), Value::Int(6)]
        )
        .unwrap(),
        vec![vec![Value::Int(50), Value::Int(6)]]
    );
    assert_eq!(
        execute(
            &mut db,
            &mut s,
            &parse("SELECT priority FROM tickets WHERE id = 50").unwrap(),
            &[]
        )
        .unwrap(),
        vec![vec![Value::Int(6)]]
    );
    execute(&mut db, &mut s, &parse("ROLLBACK").unwrap(), &[]).unwrap();
    assert_eq!(db.receipts.len(), 1);
    assert!(
        execute(
            &mut db,
            &mut s,
            &parse("SELECT id FROM tickets WHERE id = 50").unwrap(),
            &[]
        )
        .unwrap()
        .is_empty()
    );
    let update =
        parse("UPDATE tickets SET priority = priority + 1 WHERE id = $1 RETURNING id, priority")
            .unwrap();
    assert_eq!(
        execute(&mut db, &mut s, &update, &[Value::Int(2)]).unwrap(),
        vec![vec![Value::Int(2), Value::Int(3)]]
    );
    let historical = parse(&format!(
        "SELECT id, priority FROM tickets AS OF CHANGESET '{baseline}' WHERE id = 2"
    ))
    .unwrap();
    assert_eq!(
        execute(&mut db, &mut s, &historical, &[]).unwrap(),
        vec![vec![Value::Int(2), Value::Int(2)]]
    );
    // Immutable BEGIN snapshot and first-committer-wins, with no rejected WAL envelope.
    execute(&mut db, &mut s, &parse("BEGIN").unwrap(), &[]).unwrap();
    execute(&mut db, &mut s, &update, &[Value::Int(2)]).unwrap();
    let mut other = Session::default();
    execute(&mut db, &mut other, &update, &[Value::Int(2)]).unwrap();
    let before = db.receipts.len();
    let error = execute(&mut db, &mut s, &parse("COMMIT").unwrap(), &[]).unwrap_err();
    assert_eq!(
        (error.state.as_str(), error.code.as_str()),
        ("40001", "LSQL-0005")
    );
    assert_eq!(db.receipts.len(), before);
    for (sql, state, code) in [
        ("SELECT absent FROM tickets", "42703", "LSQL-0003"),
        ("SELECT id FROM missing", "42P01", "LSQL-0003"),
        (
            "INSERT INTO tickets(id, owner, priority) VALUES(2,1,1)",
            "23505",
            "LSQL-0004",
        ),
    ] {
        let err = execute(&mut db, &mut Session::default(), &parse(sql).unwrap(), &[]).unwrap_err();
        assert_eq!((err.state.as_str(), err.code.as_str()), (state, code));
    }
    for sql in [
        "SAVEPOINT s",
        "WITH q AS (SELECT id FROM tickets) SELECT id FROM q",
        "SELECT tickets.id FROM tickets FULL JOIN users ON tickets.owner = users.id",
        "SELECT row_number() OVER () FROM tickets",
    ] {
        let err = parse(sql).unwrap_err();
        assert_eq!(
            (err.state.as_str(), err.code.as_str()),
            ("0A000", "LSQL-0002"),
            "{sql}: {err}"
        );
    }
    let err = parse("SELECT id FROM").unwrap_err();
    assert_eq!(
        (err.state.as_str(), err.code.as_str()),
        ("42601", "LSQL-0001")
    );
    db.trace
}
#[test]
fn custom_all_five_constructs_and_deterministic_simulator() {
    assert_eq!(run(custom::parse), run(custom::parse));
}
#[cfg(feature = "parser")]
#[test]
fn sqlparser_all_five_constructs_and_deterministic_simulator() {
    assert_eq!(
        run(lattice_sql_planner_spike::parsed::parse),
        run(lattice_sql_planner_spike::parsed::parse)
    );
}
#[cfg(feature = "parser")]
#[test]
fn independent_parsers_lower_to_identical_plans() {
    for sql in [
        "SELECT tickets.id FROM tickets INNER JOIN users ON tickets.owner = users.id WHERE tickets.priority >= $1 ORDER BY tickets.id LIMIT 3",
        "SELECT owner, COUNT(*) FROM tickets GROUP BY owner HAVING COUNT(*) >= 2 ORDER BY owner",
        "UPDATE tickets SET priority = priority + 1 WHERE id = $1 RETURNING id, priority",
        "INSERT INTO tickets(id,owner,priority) VALUES ($1,$2,$3)",
        "SELECT id FROM tickets AS OF CHANGESET 'abc' WHERE id = 2",
    ] {
        assert_eq!(
            custom::parse(sql),
            lattice_sql_planner_spike::parsed::parse(sql),
            "{sql}"
        );
    }
}

fn optimized_relational_edges(parse: Parse) {
    let mut db = Db::open(
        SimIo::new(31, FaultProfile::clean()),
        SimClock::new(1000),
        SeededRng::new(32),
    )
    .unwrap();
    db.seed(12).unwrap();
    let baseline = db.receipts.last().unwrap().changeset.clone();
    let mut session = Session::default();
    let mut run = |sql: &str, params: &[Value]| {
        execute(&mut db, &mut session, &parse(sql).unwrap(), params).unwrap()
    };
    run(
        "INSERT INTO tickets(id,owner,priority) VALUES($1,3,9)",
        &[Value::Int(-2)],
    );
    for (sql, expected) in [
        (
            "SELECT id FROM tickets WHERE id = $1",
            vec![vec![Value::Int(-2)]],
        ),
        (
            "SELECT id FROM tickets WHERE id < 1 ORDER BY id",
            vec![vec![Value::Int(-2)], vec![Value::Int(0)]],
        ),
        (
            "SELECT id FROM tickets WHERE id >= 0 AND id < 3 ORDER BY id",
            vec![
                vec![Value::Int(0)],
                vec![Value::Int(1)],
                vec![Value::Int(2)],
            ],
        ),
        (
            "SELECT id FROM tickets WHERE 0 <= id AND 3 > id ORDER BY id",
            vec![
                vec![Value::Int(0)],
                vec![Value::Int(1)],
                vec![Value::Int(2)],
            ],
        ),
        ("SELECT id FROM tickets WHERE id > 2 AND id <= 2", vec![]),
        ("SELECT id FROM tickets WHERE id = 2 AND id = 3", vec![]),
        (
            "SELECT id FROM tickets WHERE id = $1 OR id = 2 ORDER BY id",
            vec![vec![Value::Int(-2)], vec![Value::Int(2)]],
        ),
        (
            "SELECT COUNT(*) FROM tickets WHERE id > 50",
            vec![vec![Value::Int(0)]],
        ),
        (
            "SELECT tickets.id, users.id FROM tickets INNER JOIN users ON tickets.id < users.id WHERE users.id < 2 AND tickets.id >= 0 ORDER BY tickets.id",
            vec![vec![Value::Int(0), Value::Int(1)]],
        ),
        (
            "SELECT tickets.id FROM users INNER JOIN tickets ON tickets.owner = users.id WHERE users.id = 3 ORDER BY tickets.id",
            vec![vec![Value::Int(-2)], vec![Value::Int(3)]],
        ),
    ] {
        assert_eq!(run(sql, &[Value::Int(-2)]), expected, "{sql}");
    }
    assert_eq!(
        run("SELECT id FROM tickets WHERE id = $1", &[Value::Int(2)]),
        vec![vec![Value::Int(2)]]
    );
    assert_eq!(
        run(
            "SELECT id, priority > 5 OR NULL FROM tickets ORDER BY (priority > 5 OR NULL), id LIMIT 4",
            &[]
        ),
        vec![
            vec![Value::Int(-2), Value::Bool(true)],
            vec![Value::Int(6), Value::Bool(true)],
            vec![Value::Int(0), Value::Null],
            vec![Value::Int(1), Value::Null]
        ]
    );
    assert_eq!(
        run(
            "SELECT owner + 1, COUNT(*) FROM tickets GROUP BY owner HAVING COUNT(*) > 1 ORDER BY owner + 1",
            &[]
        ),
        vec![vec![Value::Int(4), Value::Int(2)]]
    );
    assert_eq!(
        run(
            "SELECT tickets.id FROM users INNER JOIN tickets ON users.id = tickets.owner WHERE tickets.id >= 2 AND tickets.id < 4 ORDER BY tickets.id",
            &[]
        ),
        vec![vec![Value::Int(2)], vec![Value::Int(3)]]
    );
    for sql in [
        "SELECT COUNT(*) + priority FROM tickets",
        "SELECT COUNT(*) FROM tickets HAVING priority > 0",
        "SELECT id FROM tickets ORDER BY COUNT(*)",
        "SELECT id FROM tickets HAVING id = 1",
    ] {
        let error = execute(&mut db, &mut session, &parse(sql).unwrap(), &[]).unwrap_err();
        assert_eq!(
            (error.state.as_str(), error.code.as_str()),
            ("0A000", "LSQL-0002")
        );
    }
    let mut run = |sql: &str, params: &[Value]| {
        execute(&mut db, &mut session, &parse(sql).unwrap(), params).unwrap()
    };
    run("BEGIN", &[]);
    run(
        "UPDATE tickets SET priority = 123 WHERE id = 2 RETURNING id,priority",
        &[],
    );
    assert_eq!(
        run(
            "SELECT id,priority FROM tickets WHERE id >= 1 AND id <= 2 ORDER BY id",
            &[]
        ),
        vec![
            vec![Value::Int(1), Value::Int(1)],
            vec![Value::Int(2), Value::Int(123)]
        ]
    );
    run("INSERT INTO tickets(id,owner,priority) VALUES(50,3,1)", &[]);
    assert_eq!(
        run("SELECT id FROM tickets WHERE id >= 50 AND id < 51", &[]),
        vec![vec![Value::Int(50)]]
    );
    assert!(
        run(
            &format!("SELECT id FROM tickets AS OF CHANGESET '{baseline}' WHERE id = $1"),
            &[Value::Int(-2)]
        )
        .is_empty()
    );
    run("ROLLBACK", &[]);
    assert!(run("SELECT id FROM tickets WHERE id = 50", &[]).is_empty());
}
#[test]
fn custom_optimized_bounds_join_and_group_edges() {
    optimized_relational_edges(custom::parse);
}
#[cfg(feature = "parser")]
#[test]
fn sqlparser_optimized_bounds_join_and_group_edges() {
    optimized_relational_edges(lattice_sql_planner_spike::parsed::parse);
}
