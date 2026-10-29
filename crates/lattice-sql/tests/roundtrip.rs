//! Property tests: printing then parsing returns the same tree, and parsing never panics.
//!
//! Trees are generated from the AST grammar directly (so every case is valid input), and text is
//! generated from a vocabulary of SQL tokens (so deep parser paths are reached). Whatever the
//! second kind happens to parse must also survive print and re-parse. `PROPTEST_CASES` sets the
//! number of cases (proptest reads it itself).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use lattice_sql::ast::{
    AlterAction, AlterTable, AsOf, Assignment, BinaryOp, ColumnDef, CreateIndex, CreateTable,
    DataType, Delete, Expr, ExprKind, Function, FunctionArgs, Ident, Insert, InsertSource,
    IsolationLevel, Join, JoinKind, Literal, ObjectName, OrderByItem, Query, SelectItem, SetValue,
    Statement, TableConstraint, TableFactor, TableWithJoins, TrimSide, UnaryOp, Update,
};
use lattice_sql::{parse_statement, parse_statements};
use proptest::prelude::*;

// ---- generators ---------------------------------------------------------------------------------

fn ident() -> impl Strategy<Value = Ident> {
    prop_oneof![
        6 => prop::sample::select(vec!["a", "b", "id", "name", "t", "qty", "x_1", "_z"]),
        // names that must be quoted: upper case, spaces, reserved words, quotes, non-ASCII
        3 => prop::sample::select(vec!["Weird Name", "select", "from", "X", "a\"b", "über", "1st", "order"]),
    ]
    .prop_map(Ident::new)
}

fn object_name() -> impl Strategy<Value = ObjectName> {
    prop::collection::vec(ident(), 1..=2).prop_map(ObjectName)
}

fn number() -> impl Strategy<Value = String> {
    prop::sample::select(vec![
        "0", "1", "42", "3.14", "1e5", "2.5e-3", ".5", "7.", "1E+2",
    ])
    .prop_map(str::to_string)
}

fn literal() -> impl Strategy<Value = Literal> {
    prop_oneof![
        number().prop_map(Literal::Number),
        "[ -~é]{0,6}".prop_map(Literal::String),
        any::<bool>().prop_map(Literal::Boolean),
        Just(Literal::Null),
    ]
}

fn data_type() -> impl Strategy<Value = DataType> {
    prop_oneof![
        Just(DataType::Boolean),
        Just(DataType::SmallInt),
        Just(DataType::Integer),
        Just(DataType::BigInt),
        Just(DataType::DoublePrecision),
        Just(DataType::Numeric(None)),
        (1u8..=38).prop_flat_map(|p| prop_oneof![
            Just(DataType::Numeric(Some((p, None)))),
            (0..=p).prop_map(move |s| DataType::Numeric(Some((p, Some(s))))),
        ]),
        Just(DataType::Text),
        Just(DataType::Varchar(None)),
        (1u32..=10_485_760).prop_map(|n| DataType::Varchar(Some(n))),
        Just(DataType::Bytea),
        Just(DataType::Timestamptz),
        Just(DataType::Date),
        Just(DataType::Uuid),
        Just(DataType::Jsonb),
    ]
}

fn expr_of(kind: ExprKind) -> Expr {
    Expr::new(kind)
}

fn leaf_expr() -> impl Strategy<Value = Expr> {
    prop_oneof![
        3 => literal().prop_map(|l| expr_of(ExprKind::Literal(l))),
        1 => (1u32..100).prop_map(|n| expr_of(ExprKind::Param(n))),
        4 => prop::collection::vec(ident(), 1..=3).prop_map(|p| expr_of(ExprKind::Column(p))),
    ]
}

fn binary_op() -> impl Strategy<Value = BinaryOp> {
    prop::sample::select(vec![
        BinaryOp::Or,
        BinaryOp::And,
        BinaryOp::Eq,
        BinaryOp::NotEq,
        BinaryOp::Lt,
        BinaryOp::LtEq,
        BinaryOp::Gt,
        BinaryOp::GtEq,
        BinaryOp::Concat,
        BinaryOp::JsonGet,
        BinaryOp::JsonGetText,
        BinaryOp::Contains,
        BinaryOp::Add,
        BinaryOp::Sub,
        BinaryOp::Mul,
        BinaryOp::Div,
        BinaryOp::Mod,
    ])
}

fn function_name() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "abs",
        "avg",
        "coalesce",
        "date_trunc",
        "length",
        "lower",
        "max",
        "min",
        "now",
        "nullif",
        "round",
        "substring",
        "sum",
        "upper",
    ])
}

/// A query built from leaf expressions only, usable inside expressions.
fn small_query() -> impl Strategy<Value = Query> {
    (
        prop::collection::vec(leaf_expr(), 1..3),
        prop::option::of(object_name()),
        prop::option::of(leaf_expr()),
    )
        .prop_map(|(items, table, selection)| Query {
            distinct: false,
            projection: items
                .into_iter()
                .map(|expr| SelectItem::Expr { expr, alias: None })
                .collect(),
            from: table
                .into_iter()
                .map(|name| TableWithJoins {
                    relation: TableFactor {
                        name,
                        as_of: None,
                        alias: None,
                    },
                    joins: Vec::new(),
                })
                .collect(),
            selection,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        })
}

fn expr() -> impl Strategy<Value = Expr> {
    leaf_expr().prop_recursive(5, 48, 3, |inner| {
        let b = |e: Expr| Box::new(e);
        prop_oneof![
            (
                prop::sample::select(vec![UnaryOp::Plus, UnaryOp::Minus, UnaryOp::Not]),
                inner.clone()
            )
                .prop_map(move |(op, e)| expr_of(ExprKind::Unary { op, expr: b(e) })),
            (binary_op(), inner.clone(), inner.clone()).prop_map(move |(op, l, r)| expr_of(
                ExprKind::Binary {
                    op,
                    left: b(l),
                    right: b(r)
                }
            )),
            (inner.clone(), any::<bool>()).prop_map(move |(e, negated)| expr_of(
                ExprKind::IsNull {
                    expr: b(e),
                    negated
                }
            )),
            (
                inner.clone(),
                prop::collection::vec(inner.clone(), 1..4),
                any::<bool>()
            )
                .prop_map(move |(e, list, negated)| expr_of(ExprKind::InList {
                    expr: b(e),
                    list,
                    negated
                })),
            (inner.clone(), small_query(), any::<bool>()).prop_map(move |(e, q, negated)| {
                expr_of(ExprKind::InSubquery {
                    expr: b(e),
                    query: Box::new(q),
                    negated,
                })
            }),
            (inner.clone(), inner.clone(), inner.clone(), any::<bool>()).prop_map(
                move |(e, low, high, negated)| expr_of(ExprKind::Between {
                    expr: b(e),
                    low: b(low),
                    high: b(high),
                    negated
                })
            ),
            (inner.clone(), inner.clone(), any::<bool>()).prop_map(move |(e, p, negated)| expr_of(
                ExprKind::Like {
                    expr: b(e),
                    pattern: b(p),
                    negated
                }
            )),
            (function_name(), prop::collection::vec(inner.clone(), 0..4)).prop_map(
                |(name, args)| expr_of(ExprKind::Function(Function {
                    name: name.to_string(),
                    distinct: false,
                    args: FunctionArgs::List(args)
                }))
            ),
            (inner.clone(), any::<bool>()).prop_map(|(e, star)| {
                if star {
                    expr_of(ExprKind::Function(Function {
                        name: "count".into(),
                        distinct: false,
                        args: FunctionArgs::Star,
                    }))
                } else {
                    expr_of(ExprKind::Function(Function {
                        name: "count".into(),
                        distinct: true,
                        args: FunctionArgs::List(vec![e]),
                    }))
                }
            }),
            (inner.clone(), data_type()).prop_map(move |(e, data_type)| expr_of(ExprKind::Cast {
                expr: b(e),
                data_type
            })),
            small_query().prop_map(|q| expr_of(ExprKind::Subquery(Box::new(q)))),
            (inner.clone(), inner.clone()).prop_map(move |(n, h)| expr_of(ExprKind::Position {
                needle: b(n),
                haystack: b(h)
            })),
            (
                prop::sample::select(vec![TrimSide::Both, TrimSide::Leading, TrimSide::Trailing]),
                prop::option::of(inner.clone()),
                inner.clone()
            )
                .prop_map(move |(side, chars, e)| expr_of(ExprKind::Trim {
                    side,
                    chars: chars.map(Box::new),
                    expr: b(e)
                })),
            (ident(), inner)
                .prop_map(move |(field, e)| expr_of(ExprKind::Extract { field, expr: b(e) })),
        ]
    })
}

fn constant() -> impl Strategy<Value = Expr> {
    prop_oneof![
        literal().prop_map(|l| expr_of(ExprKind::Literal(l))),
        (
            prop::sample::select(vec![UnaryOp::Plus, UnaryOp::Minus]),
            number()
        )
            .prop_map(|(op, n)| {
                expr_of(ExprKind::Unary {
                    op,
                    expr: Box::new(expr_of(ExprKind::Literal(Literal::Number(n)))),
                })
            }),
        (literal(), data_type()).prop_map(|(l, data_type)| expr_of(ExprKind::Cast {
            expr: Box::new(expr_of(ExprKind::Literal(l))),
            data_type
        })),
    ]
}

fn select_item() -> impl Strategy<Value = SelectItem> {
    prop_oneof![
        Just(SelectItem::Wildcard),
        ident().prop_map(SelectItem::QualifiedWildcard),
        (expr(), prop::option::of(ident()))
            .prop_map(|(expr, alias)| SelectItem::Expr { expr, alias }),
    ]
}

fn table_factor() -> impl Strategy<Value = TableFactor> {
    let operand = prop_oneof![
        number().prop_map(|n| expr_of(ExprKind::Literal(Literal::Number(n)))),
        "[ -~]{0,8}".prop_map(|s| expr_of(ExprKind::Literal(Literal::String(s)))),
        (1u32..50).prop_map(|n| expr_of(ExprKind::Param(n))),
    ];
    (
        object_name(),
        prop::option::of((any::<bool>(), operand).prop_map(|(c, e)| {
            if c {
                AsOf::Changeset(Box::new(e))
            } else {
                AsOf::Timestamp(Box::new(e))
            }
        })),
        prop::option::of(ident()),
    )
        .prop_map(|(name, as_of, alias)| TableFactor { name, as_of, alias })
}

fn table_with_joins() -> impl Strategy<Value = TableWithJoins> {
    let join = (
        prop::sample::select(vec![JoinKind::Inner, JoinKind::Left, JoinKind::Cross]),
        table_factor(),
        expr(),
    )
        .prop_map(|(kind, relation, on)| Join {
            kind,
            relation,
            on: (kind != JoinKind::Cross).then_some(on),
        });
    (table_factor(), prop::collection::vec(join, 0..3))
        .prop_map(|(relation, joins)| TableWithJoins { relation, joins })
}

fn query() -> impl Strategy<Value = Query> {
    let order = (expr(), any::<bool>(), prop::option::of(any::<bool>())).prop_map(
        |(expr, desc, nulls_first)| OrderByItem {
            expr,
            desc,
            nulls_first,
        },
    );
    (
        any::<bool>(),
        prop::collection::vec(select_item(), 1..4),
        prop::collection::vec(table_with_joins(), 0..3),
        prop::option::of(expr()),
        prop::collection::vec(expr(), 0..3),
        prop::option::of(expr()),
        prop::collection::vec(order, 0..3),
        prop::option::of(expr()),
        prop::option::of(expr()),
    )
        .prop_map(
            |(distinct, projection, from, selection, group_by, having, order_by, limit, offset)| {
                Query {
                    distinct,
                    projection,
                    from,
                    selection,
                    group_by,
                    having,
                    order_by,
                    limit,
                    offset,
                }
            },
        )
}

fn column_def() -> impl Strategy<Value = ColumnDef> {
    (
        ident(),
        data_type(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        prop::option::of(constant()),
    )
        .prop_map(
            |(name, data_type, not_null, primary_key, unique, default)| ColumnDef {
                name,
                data_type,
                not_null,
                primary_key,
                unique,
                default,
            },
        )
}

fn returning() -> impl Strategy<Value = Vec<SelectItem>> {
    prop::collection::vec(select_item(), 0..3)
}

fn statement() -> impl Strategy<Value = Statement> {
    let insert = (
        object_name(),
        prop::collection::vec(ident(), 0..3),
        prop_oneof![
            (1usize..4, 1usize..4).prop_flat_map(|(w, h)| {
                prop::collection::vec(prop::collection::vec(expr(), w), h)
                    .prop_map(InsertSource::Values)
            }),
            query().prop_map(|q| InsertSource::Query(Box::new(q))),
        ],
        returning(),
    )
        .prop_map(|(table, columns, source, returning)| {
            Statement::Insert(Insert {
                table,
                columns,
                source,
                returning,
            })
        });
    let update = (
        object_name(),
        prop::option::of(ident()),
        prop::collection::vec(
            (ident(), expr()).prop_map(|(column, value)| Assignment { column, value }),
            1..3,
        ),
        prop::option::of(expr()),
        returning(),
    )
        .prop_map(|(table, alias, assignments, selection, returning)| {
            Statement::Update(Update {
                table,
                alias,
                assignments,
                selection,
                returning,
            })
        });
    let delete = (
        object_name(),
        prop::option::of(ident()),
        prop::option::of(expr()),
        returning(),
    )
        .prop_map(|(table, alias, selection, returning)| {
            Statement::Delete(Delete {
                table,
                alias,
                selection,
                returning,
            })
        });
    let create_table = (
        object_name(),
        any::<bool>(),
        prop::collection::vec(column_def(), 1..4),
        prop::collection::vec(
            (any::<bool>(), prop::collection::vec(ident(), 1..3)).prop_map(|(pk, cols)| {
                if pk {
                    TableConstraint::PrimaryKey(cols)
                } else {
                    TableConstraint::Unique(cols)
                }
            }),
            0..3,
        ),
    )
        .prop_map(|(name, if_not_exists, columns, constraints)| {
            Statement::CreateTable(CreateTable {
                name,
                if_not_exists,
                columns,
                constraints,
            })
        });
    let alter = (
        object_name(),
        prop_oneof![
            column_def().prop_map(AlterAction::AddColumn),
            ident().prop_map(AlterAction::DropColumn),
            (ident(), ident()).prop_map(|(from, to)| AlterAction::RenameColumn { from, to }),
            ident().prop_map(AlterAction::RenameTable),
        ],
    )
        .prop_map(|(name, action)| Statement::AlterTable(AlterTable { name, action }));
    let create_index = (
        prop::option::of(ident()),
        any::<bool>(),
        any::<bool>(),
        object_name(),
        prop::collection::vec(ident(), 1..4),
    )
        .prop_map(|(name, unique, if_not_exists, table, columns)| {
            Statement::CreateIndex(CreateIndex {
                name,
                unique,
                if_not_exists,
                table,
                columns,
            })
        });
    let level = prop::sample::select(vec![
        IsolationLevel::RepeatableRead,
        IsolationLevel::Serializable,
    ]);
    let set_value = prop_oneof![
        Just(None),
        prop::sample::select(vec!["0", "42", "-1", "-2.5"])
            .prop_map(|n| Some(SetValue::Number(n.to_string()))),
        "[ -~]{0,6}".prop_map(|s| Some(SetValue::String(s))),
        prop::sample::select(vec!["on", "off", "utf8", "read", "select"])
            .prop_map(|w| Some(SetValue::Word(w.to_string()))),
    ];
    prop_oneof![
        3 => query().prop_map(|q| Statement::Query(Box::new(q))),
        2 => insert,
        2 => update,
        2 => delete,
        2 => create_table,
        1 => alter,
        1 => (object_name(), any::<bool>())
            .prop_map(|(name, if_exists)| Statement::DropTable { name, if_exists }),
        1 => create_index,
        1 => (object_name(), any::<bool>())
            .prop_map(|(name, if_exists)| Statement::DropIndex { name, if_exists }),
        1 => prop::option::of(level.clone()).prop_map(|isolation| Statement::Begin { isolation }),
        1 => Just(Statement::Commit),
        1 => Just(Statement::Rollback),
        1 => level.prop_map(|isolation| Statement::SetTransaction { isolation }),
        1 => (ident(), set_value).prop_map(|(name, value)| Statement::Set { name, value }),
        1 => ident().prop_map(|name| Statement::Show { name }),
    ]
}

// ---- properties ---------------------------------------------------------------------------------

proptest! {
    /// parse(print(ast)) == ast for every generated statement.
    #[test]
    fn print_then_parse_returns_the_tree(stmt in statement()) {
        let text = stmt.to_string();
        let parsed = parse_statement(&text);
        prop_assert_eq!(parsed.as_ref(), Ok(&stmt), "text: {}", text);
    }

    /// Printing is a fixed point: the canonical text prints as itself.
    #[test]
    fn printing_is_stable(stmt in statement()) {
        let text = stmt.to_string();
        let again = parse_statement(&text).map(|s| s.to_string());
        prop_assert_eq!(again, Ok(text));
    }

    /// A generated expression keeps its tree through select-list position.
    #[test]
    fn expressions_round_trip(e in expr()) {
        let stmt = Statement::Query(Box::new(Query {
            distinct: false,
            projection: vec![SelectItem::Expr { expr: e, alias: None }],
            from: Vec::new(),
            selection: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        }));
        let text = stmt.to_string();
        let parsed = parse_statement(&text);
        prop_assert_eq!(parsed.as_ref(), Ok(&stmt), "text: {}", text);
    }

    /// Arbitrary text never panics, and anything that parses survives print and re-parse.
    #[test]
    fn arbitrary_text_never_panics(s in "\\PC{0,80}") {
        check_idempotent(&s)?;
    }

    /// A valid statement with one token deleted or inserted is near-valid input: usually an error,
    /// sometimes another valid statement. Either way no panic, and valid results are stable.
    #[test]
    fn edited_statements_never_panic(
        stmt in statement(),
        at in any::<prop::sample::Index>(),
        token in prop::sample::select(VOCAB.to_vec()),
        insert in any::<bool>(),
    ) {
        let text = stmt.to_string();
        let mut words: Vec<&str> = text.split(' ').collect();
        let i = at.index(words.len());
        if insert {
            words.insert(i, token);
        } else {
            words.remove(i);
        }
        check_idempotent(&words.join(" "))?;
    }

    /// Token soup reaches the deep parser paths; it must hold the same two properties.
    #[test]
    fn token_soup_never_panics(tokens in prop::collection::vec(prop::sample::select(VOCAB.to_vec()), 0..40)) {
        check_idempotent(&tokens.join(" "))?;
    }
}

fn check_idempotent(text: &str) -> Result<(), TestCaseError> {
    if let Ok(stmts) = parse_statements(text) {
        for stmt in stmts {
            let printed = stmt.to_string();
            let reparsed = parse_statement(&printed);
            prop_assert_eq!(
                reparsed.as_ref(),
                Ok(&stmt),
                "input: {} printed: {}",
                text,
                printed
            );
        }
    }
    Ok(())
}

const VOCAB: &[&str] = &[
    "SELECT",
    "DISTINCT",
    "FROM",
    "WHERE",
    "GROUP",
    "BY",
    "HAVING",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "NULLS",
    "FIRST",
    "LAST",
    "ASC",
    "DESC",
    "AS",
    "OF",
    "CHANGESET",
    "TIMESTAMP",
    "JOIN",
    "INNER",
    "LEFT",
    "OUTER",
    "CROSS",
    "RIGHT",
    "ON",
    "AND",
    "OR",
    "NOT",
    "IN",
    "BETWEEN",
    "LIKE",
    "IS",
    "NULL",
    "TRUE",
    "FALSE",
    "INSERT",
    "INTO",
    "VALUES",
    "RETURNING",
    "UPDATE",
    "SET",
    "DELETE",
    "CREATE",
    "TABLE",
    "INDEX",
    "UNIQUE",
    "PRIMARY",
    "KEY",
    "DEFAULT",
    "IF",
    "EXISTS",
    "ALTER",
    "ADD",
    "COLUMN",
    "DROP",
    "RENAME",
    "TO",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "START",
    "TRANSACTION",
    "ISOLATION",
    "LEVEL",
    "SERIALIZABLE",
    "REPEATABLE",
    "READ",
    "SHOW",
    "CAST",
    "position",
    "trim",
    "extract",
    "substring",
    "BOTH",
    "LEADING",
    "count",
    "sum",
    "coalesce",
    "now",
    "int",
    "text",
    "numeric",
    "varchar",
    "double",
    "precision",
    "timestamp",
    "with",
    "time",
    "zone",
    "date",
    "a",
    "b",
    "t",
    "\"Q\"",
    "x1",
    "1",
    "2.5",
    ".5",
    "'s'",
    "$1",
    "(",
    ")",
    ",",
    ";",
    ".",
    "*",
    "+",
    "-",
    "/",
    "%",
    "=",
    "<>",
    "<",
    ">=",
    "||",
    "->",
    "->>",
    "@>",
    "::",
    "--",
    "/*",
];
