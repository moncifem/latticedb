The negative file uses sqllogictest's statement/error record form with the experiment's exact
SQLSTATE and LSQL pair as its expected error pattern. The local runner implements this record form;
it is not a claim that the full production sqllogictest conformance suite exists. Valid PostgreSQL
outside the measured prototype grammar is rejected, including target constructs not implemented in
this spike. The full target remains docs/sql-subset.md, not this small fixture set.
