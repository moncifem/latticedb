# tla

TLA+ models checked by TLC. Each header states what the model covers and what it leaves out.

| Model | Property |
|---|---|
| `Commit` | an acknowledged change set is durable and survives crashes |
| `Hlc` | timestamps strictly increase even when the physical clock goes backwards |
| `Fence` | no write from an older turn is accepted once a newer turn has begun |
| `SetConvergence` | replicas that delivered the same operations show the same members |

Run one with `tla/run-tlc.sh tla/<Model>.tla`, or all with `tla/check-all.sh`. Both need `java` and
`TLA2TOOLS` pointing at `tla2tools.jar`. Nightly CI runs all of them.
