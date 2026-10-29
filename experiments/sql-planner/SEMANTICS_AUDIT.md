# Bounded expression and execution audit

This reviews the experiment's five representative constructs and its small AST, not the complete
SQL subset or a production SQL binder. It does not accept ADR 0013. The fixed fixture schema is
`tickets(id bigint, owner bigint, priority bigint)` and `users(id bigint, name text)`, all NOT NULL.

| Boundary | Accepted behavior and checks |
|---|---|
| Names | Both front ends lower to the same AST. The shared binder validates qualified/unqualified columns and removes ambiguous unqualified join names. Unsupported aliases and self joins are refused. |
| Values | Only signed bigint, text, boolean and NULL exist. The shared expression check resolves the fixture column kinds and actual parameter values before execution, including on empty inputs. There are no implicit casts or general PostgreSQL type inference. |
| Parameters | The highest referenced parameter position must match the supplied array length; missing and extra values fail. Explicit typed integer/text/boolean/NULL transport preserves types. Prepared reuse revalidates each bind before executing the cached plan. |
| Arithmetic | `+` and `-` require bigint operands or typed NULL, with at least one bigint. Runtime arithmetic is checked for overflow. Unsupported coercions and ambiguous NULL-only arithmetic are refused. This is not complete PostgreSQL constant folding or numeric-error fidelity. |
| Comparisons | `=`, `<`, `>`, `<=`, `>=` require matching kinds, permitting NULL. NULL comparisons retain three-valued behavior. Known mixed-type column joins are refused before equality-index selection could hide the error by producing no candidates. |
| Predicates | WHERE, JOIN ON and HAVING require boolean or NULL at bind/type validation and runtime. AND/OR require boolean/NULL operands, including actual parameter types, so a NULL or short-circuit result cannot conceal an integer operand. |
| Grouping | Projection, HAVING and ORDER expressions recursively reference group keys, constants, parameters or COUNT. HAVING and ORDER COUNT classify aggregate queries. Ungrouped references, and aggregates in WHERE/GROUP/JOIN ON, are explicitly unsupported. Functional dependencies and equivalence of differently qualified group expressions are not inferred. |
| Ordering | Default ASC uses NULLS LAST with lexicographic secondary keys. The DataFusion renderer makes that order explicit. DESC, explicit user NULLS FIRST/LAST and general collation configuration remain outside the prototype grammar. The measured schema uses ASCII fixture names; no general PostgreSQL collation proof is claimed. |
| Join optimization | Only proven equality keys use the deterministic index. Table-local conjuncts may be evaluated on an input first; complete ON/WHERE predicates still run afterward. General supported predicates retain the fallback loop. Duplicate keys and NULL behavior have independent literal cases. |
| Scan optimization | Conservative primary-key bounds retain a full residual predicate. Signed-negative key ordering, reversed comparisons, contradictions, OR fallback, pending replacements/inserts and historical views are tested. Ordered snapshot rows and actual pending rows merge without a second full key map. No result/decoded-row cache or fixture answer is substituted. |
| Writes | Assignment kinds are checked before staging, NULL still reaches NOT NULL validation, UPDATE assignments use the old row simultaneously, and duplicate INSERT keys are checked against the transaction view. All candidates publish through one RowTx/WAL change set. |
| Publication oracle | Session.pending mirrors submitted executor rows and independently checks RowTx publication fidelity, not SQL choice correctness. Complete PostgreSQL results, literal conformance expectations and input-derived process-kill WAL/full-state checks remain independent SQL oracles. |

The cached DataFusion INSERT stores its original bounded AST so every actual parameter rebind is
checked against the same contract. DataFusion still plans and executes its relational projections;
these guards do not substitute custom relational execution. Custom/parser and DataFusion transaction
control share the real storage bridge.

Remaining scope gaps include the PostgreSQL aborted-transaction session state, full type/coercion and
collation semantics, general prepared statements, aliases, LEFT JOIN, DELETE, DDL and the rest of the
declared subset. Invalid forms within the bounded AST are refused; successful cases in this document
are not full-subset coverage. C1's missing budget/historical effort and the qualified C7 assessment
remain open.

## Cheap semantic confirmation

Dispatch `io-probe.yml` on the spike branch with `sql_planner=true` and `semantic_only=true`.
`semantic_confirmation` executes custom/parser builds, conformance, simulator and PostgreSQL semantic
checks, plus the process-kill cases. It never builds DataFusion, never runs the four-query benchmark,
never supplies clean-build evidence and explicitly suppresses C4/C5 criterion assessment. The raw
policy checks remain diagnostic logs. Default full measurement still selects all three candidates,
uses all four unchanged benchmarks and retains clean-target behavior unless functional retry is
explicitly chosen. Final all-candidate confirmation follows code freeze and independent review.
