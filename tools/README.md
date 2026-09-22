# Running performance experiments

Run builds and measurements on the benchmark node. Keep compilation separate
from timed runs. The Python runner requires Linux, Python 3.11+, GNU time,
coreutils, util-linux, and Git. It has no Python package dependencies.

Build the command and the library harness before starting measurements:

```bash
cargo build --locked --release -p tarseer-cli
cargo build --locked --release --example walkbench --no-default-features
target/release/examples/walkbench walk /var/tmp/tarseer-fixtures/tree 4194304 8
```

The harness takes `<walk|json> <tree> <budget-bytes> <threads>
[walk|completion]`. It reports the entry and part counts on stderr. `walk`
consumes parts; `json` writes each part into a reused buffer. It uses the system
allocator. The CLI uses the system allocator on Linux. Zero walk threads selects
the serial walker; eight selects eight workers plus the calling thread.

Save each build under a separate name before editing sources. Compare those
binaries in an experiment document:

```json
{
  "version": 1,
  "passes": 11,
  "cpus": "0-7",
  "timeout_seconds": 300,
  "cold": false,
  "groups": [{
    "name": "tree",
    "cases": [
      {
        "label": "baseline",
        "command": ["/var/tmp/baseline", "walk", "/var/tmp/tarseer-fixtures/tree", "4194304", "8"],
        "expected_entries": 181240
      },
      {
        "label": "candidate",
        "command": ["/var/tmp/candidate", "walk", "/var/tmp/tarseer-fixtures/tree", "4194304", "8"],
        "expected_entries": 181240
      }
    ]
  }]
}
```

Run it with a new output directory:

```bash
python3 tools/benchmark.py experiment.json /var/tmp/my-results
```

The runner warms each case, then rotates the case order between passes. It pins
each command to the listed CPUs. Exit failures and incorrect entry counts stop
the run after saving the failing sample. An expected count requires exactly one
`N entries` report on stderr. Omit that field for commands with other output.
Commands are argument arrays executed without a shell.

Results include raw JSON Lines, median/minimum/maximum times, peak RSS, CPU time,
context switches, faults, I/O, guest steal time, executable hashes, and machine
metadata. The wall time includes process launch and measurement wrappers.
GNU time's CPU times have hundredth-second precision. RSS summaries report the
largest observed process peak. Each group has a warmup row per case and one row
per timed sample. A `complete` marker appears only after every group succeeds.

The runner refuses to overwrite an output directory. It also refuses to run
while another instance holds `/var/tmp/tarseer-benchmark.lock`. This lock does
not detect unrelated jobs; keep builds, fixture creation and profilers idle
during timings. Save the source archive or patch with the results; executable
hashes alone do not identify uncommitted changes.

`cold: true` runs `sync` and drops the guest's page, dentry and inode caches before
every sample, using passwordless sudo. It affects the whole guest. This does
not evict the hypervisor's or storage device's caches.

## Running on netcup

The sibling infrastructure repository provides `infra/netcup-node/netcup.ts`.
From that directory:

```bash
./netcup.ts scp /tmp/source.tar netcup:/var/tmp/
./netcup.ts start /var/tmp/my-experiment < /tmp/job.sh
./netcup.ts status /var/tmp/my-experiment
./netcup.ts logs /var/tmp/my-experiment
./netcup.ts scp -r netcup:/var/tmp/my-experiment /tmp/
```

Put build and measurement commands in `job.sh`, beginning with `set -euo pipefail`.
The helper saves that script and runs it in the background with a log and exit
status. Give each experiment a new directory. Use a host lock around the entire
job when another experiment could be compiling at the same time:

```bash
set -euo pipefail
exec 9>/var/tmp/tarseer-experiment.lock
flock -n 9
cd /var/tmp/my-source/tarseer
export CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
cargo build --locked --release --example walkbench --no-default-features
python3 tools/benchmark.py /var/tmp/experiment.json /var/tmp/my-experiment/results
```

`tarseer.dev/notes/benchmarking` contains fixture generators, zlob comparators,
the extended harness and a netcup job template. The dated netcup reports retain
the original measurements and the exact commands used for each experiment.

## Profiling a candidate

Profile after timing. Keep debug line tables in release builds and record kernel
and user events separately. For example, where the node's perf policy permits:

```bash
perf stat -r 5 -e instructions:u,instructions:k,cycles:u,cycles:k,context-switches,page-faults -- \
  taskset -c 0-7 /var/tmp/candidate walk /var/tmp/tarseer-fixtures/uniform 4194304 8
perf record -o /var/tmp/candidate.data -e cycles -g --call-graph dwarf -- \
  taskset -c 0-7 /var/tmp/candidate walk /var/tmp/tarseer-fixtures/uniform 4194304 8
perf report -i /var/tmp/candidate.data --stdio > /var/tmp/candidate-profile.txt
```

Check which hardware events the VM supports. Guest cache counters do not reveal
physical cache topology. Cachegrind models userspace cache behavior and excludes
kernel work and parallel coherence. Record its configured cache geometry.

An unordered callback and a deterministic, bounded manifest perform different
work. Report both throughput and RSS, and state sorting, metadata, symlink and
output semantics for each comparator. Check byte identity before timing changes
that affect scheduling or part boundaries.
