#!/usr/bin/env python3
"""Manual CI experiment. Missing/failed candidate evidence is retained, never filled with estimates."""
import argparse, hashlib, json, os, pathlib, platform, queue, signal, shutil, statistics, subprocess, sys, tempfile, threading, time, traceback

from criteria import assess

HERE = pathlib.Path(__file__).resolve().parent
MANIFEST = HERE / 'Cargo.toml'
RESULT_CODEC = 'json-scalars-v1'
FEATURES = {'custom': [], 'parser': ['parser'], 'datafusion': ['datafusion']}
QUERIES = {
    'point': 'SELECT id, owner, priority FROM tickets WHERE id = 1234',
    'range_10000': 'SELECT id, owner, priority FROM tickets WHERE id >= 5000 AND id < 15000 ORDER BY id',
    'join': 'SELECT tickets.id, users.name FROM tickets INNER JOIN users ON tickets.owner = users.id WHERE tickets.priority >= 5 ORDER BY tickets.id',
    'group': 'SELECT owner, COUNT(*) FROM tickets GROUP BY owner HAVING COUNT(*) >= 1 ORDER BY owner',
}

def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')

def command(args, output, timeout=1800, env=None):
    start = time.monotonic()
    with output.open('w') as log:
        process = subprocess.Popen(args, cwd=HERE, stdout=log, stderr=subprocess.STDOUT,
                                   env=env, start_new_session=(os.name=='posix'))
        try:
            process.wait(timeout=timeout)
            return {'command': list(map(str,args)), 'exit': process.returncode,
                    'elapsed_s': time.monotonic()-start, 'log': output.name}
        except subprocess.TimeoutExpired:
            if os.name=='posix':
                try:os.killpg(process.pid,signal.SIGKILL)
                except ProcessLookupError:pass
            else:
                # The measurement workflow is Linux; retain a tree-kill path for Windows callers.
                subprocess.run(['taskkill','/PID',str(process.pid),'/T','/F'],stdout=log,stderr=subprocess.STDOUT,check=False)
                if process.poll() is None:process.kill()
            process.wait(timeout=30)
            return {'command': list(map(str,args)), 'exit': process.returncode,
                    'timed_out': True, 'whole_process_group_killed': True,
                    'elapsed_s': time.monotonic()-start, 'log': output.name}

def reachable_packages(metadata, kinds=(None,)):
    nodes={node['id']:node for node in metadata['resolve']['nodes']}
    pending=[metadata['resolve']['root']];reachable=set()
    while pending:
        package_id=pending.pop()
        if package_id in reachable:continue
        reachable.add(package_id)
        pending += [dep['pkg']for dep in nodes[package_id]['deps']if any(kind['kind'] in kinds for kind in dep['dep_kinds'])]
    return reachable

def source_hints(metadata_path, output):
    metadata=json.loads(metadata_path.read_text())
    symbols=['RandomState','RandomizedHashBuilder','SystemTime::now','Instant::now','tokio::spawn','std::thread::spawn','HashMap::new']
    hints=[];total=0
    reachable=reachable_packages(metadata)
    for package in metadata['packages']:
        if package['id'] not in reachable or package.get('source') is None:continue
        root=pathlib.Path(package['manifest_path']).parent
        for path in root.rglob('*.rs'):
            for line_number,line in enumerate(path.read_text(errors='replace').splitlines(),1):
                matched=[symbol for symbol in symbols if symbol in line]
                if matched:
                    total+=1
                    if len(hints)<2000:hints.append({'crate':package['name'],'version':package['version'],'file':str(path.relative_to(root)),'line':line_number,'symbols':matched,'source':line.strip()})
    value={'status':'unreviewed source hints, not proof of reachable nondeterministic execution','total_matching_lines':total,'truncated':total>len(hints),'hints':hints}
    save(output,value)
    return {'status':'manual call-path review required','hint_file':output.name,'total_matching_lines':total,'truncated':value['truncated']}

def scalar_rows(rows):
    if type(rows) is not list or any(type(row) is not list for row in rows):
        raise ValueError('result must be a complete row array')
    if any(type(value) not in (type(None), int, str, bool) for row in rows for value in row):
        raise ValueError('unexpected scalar result type')
    if any(type(value) is int and not -(2**63) <= value < 2**63 for row in rows for value in row):
        raise ValueError('result integer exceeds signed bigint')
    return rows


def same_rows(actual, expected):
    return (len(actual) == len(expected) and all(
        len(left) == len(right) and all(type(a) is type(b) and a == b for a,b in zip(left,right))
        for left,right in zip(actual,expected)))

class Host:
    def __init__(self, binary, candidate, root, rows, log):
        self.stderr = log.open('w')
        self.proc = subprocess.Popen([str(binary), '--candidate', candidate, '--mode','serve','--dir',str(root),'--rows',str(rows)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr, text=True, bufsize=1,start_new_session=(os.name=='posix'))
        self.events = queue.Queue()
        def read():
            try:
                for line in self.proc.stdout:
                    if not line.endswith('\n'):
                        self.events.put({'unpublished_tail': line})
                        break
                    self.events.put(json.loads(line))
            except BaseException as e:
                self.events.put({'protocol_error': repr(e)})
            finally:
                self.events.put(None)
        self.reader = threading.Thread(target=read)
        self.reader.start()
        try:
            self.ready = self.receive()
            if self.ready.get('result_codec') != RESULT_CODEC:
                raise RuntimeError('candidate result codec differs from coordinator')
            if not self.ready.get('ready'):
                raise RuntimeError(f'no ready message: {self.ready}')
        except BaseException:
            self.close(kill=True)
            raise
    def receive(self):
        value = self.events.get(timeout=60)
        if value is None:
            raise RuntimeError(f'host closed stdout, status={self.proc.poll()}')
        if 'protocol_error' in value or 'unpublished_tail' in value:
            raise RuntimeError(value)
        return value
    def submit(self, sql, params=(), prepared=False, commit_barrier=None):
        request={'sql':sql,'params':[{'Int':n}for n in params],'prepared':prepared}
        if commit_barrier is not None:request['commit_barrier']=commit_barrier
        self.proc.stdin.write(json.dumps(request)+'\n')
        self.proc.stdin.flush()
    def execute(self, sql, params=(), prepared=False):
        start=time.perf_counter_ns();self.submit(sql,params,prepared);reply=self.receive()
        if 'Err' in reply['result']:
            raise RuntimeError(reply['result']['Err'])
        elapsed=time.perf_counter_ns()-start
        rows=scalar_rows(reply['result']['Ok'])
        return rows,elapsed,reply['engine_ns']
    def kill_group(self):
        if self.proc.poll() is None:
            if os.name=='posix':os.killpg(self.proc.pid,signal.SIGKILL)
            else:
                subprocess.run(['taskkill','/PID',str(self.proc.pid),'/T','/F'],stdout=self.stderr,stderr=self.stderr,check=False)
                if self.proc.poll() is None:self.proc.kill()
    def close(self, kill=False):
        try:
            if kill and self.proc.poll() is None:
                self.kill_group()
            else:
                self.proc.stdin.close()
            self.proc.wait(timeout=10)
        except BaseException:
            self.kill_group();self.proc.wait(timeout=10)
            raise
        finally:
            self.reader.join(timeout=10)
            self.stderr.close()
            if self.reader.is_alive():
                raise RuntimeError('reader failed to reach EOF')
    def __enter__(self):return self
    def __exit__(self,*exc):self.close(kill=bool(exc[0]))

def summary(samples):
    ordered=sorted(samples)
    return {'samples_ns':samples, 'median_ns':statistics.median(samples), 'p95_ns':ordered[min(len(ordered)-1,int(len(ordered)*.95))]}

def representatives(binary,candidate,root,output,dsn,rows):
    import psycopg, re
    cases=[]
    with psycopg.connect(dsn,autocommit=True) as pg:
        pg.execute('DROP TABLE IF EXISTS tickets,users,tickets_at_seed')
        pg.execute('CREATE TABLE users(id bigint PRIMARY KEY,name text NOT NULL)')
        pg.execute('CREATE TABLE tickets(id bigint PRIMARY KEY,owner bigint NOT NULL,priority bigint NOT NULL)')
        pg.execute("INSERT INTO users SELECT i,'user'||lpad(i::text,3,'0') FROM generate_series(0,99) i")
        pg.execute('INSERT INTO tickets SELECT i,mod(i,100),mod(i,7) FROM generate_series(0,%s) i',(rows-1,))
        pg.execute('CREATE TABLE tickets_at_seed AS SELECT * FROM tickets')
        pg.execute("SET default_transaction_isolation='repeatable read'")
        with Host(binary,candidate,root,rows,output/f'{candidate}-representatives.stderr') as host:
            assert not host.ready['fault_hooks'],'performance/positive path must not compile fault hooks'
            baseline=host.ready['receipts'][0]['changeset']
            def run(name,sql,params=(),prepared=False,reference=None):
                actual,elapsed,engine=host.execute(sql,params,prepared)
                reference=reference or sql
                positions=[]
                def placeholder(match):
                    positions.append(params[int(match.group(1))-1]);return '%s'
                pgsql=re.sub(r'\$(\d+)',placeholder,reference)
                cursor=pg.execute(pgsql,positions or None,prepare=prepared)
                expected=[list(r)for r in cursor.fetchall()] if cursor.description else []
                assert same_rows(actual,expected),(candidate,name,actual[:5],expected[:5])
                cases.append({'construct':name,'sql':sql,'postgres_sql':reference,'params':params,'prepared':prepared,'client_ns':elapsed,'engine_ns':engine,'row_count':len(actual),'results_match':True})
            run('join_filter','SELECT tickets.id, users.name FROM tickets INNER JOIN users ON tickets.owner = users.id WHERE tickets.priority >= $1 ORDER BY tickets.id LIMIT 5',(5,))
            run('ascending_nulls_last','SELECT id, priority > 5 OR NULL FROM tickets ORDER BY (priority > 5 OR NULL), id LIMIT 8')
            run('group_having','SELECT owner, COUNT(*) FROM tickets GROUP BY owner HAVING COUNT(*) >= $1 ORDER BY owner',(1,))
            run('update_returning','UPDATE tickets SET priority = priority + 1 WHERE id = $1 RETURNING id, priority',(2,))
            run('as_of',f"SELECT id, priority FROM tickets AS OF CHANGESET '{baseline}' WHERE id = $1",(2,),reference='SELECT id, priority FROM tickets_at_seed WHERE id = $1')
            run('begin','BEGIN')
            insert='INSERT INTO tickets(id,owner,priority) VALUES($1,$2,$3) RETURNING id,priority'
            run('prepared_insert',insert,(50000,3,4),True)
            run('prepared_insert_rebind',insert,(50001,4,5),True)
            run('own_writes','SELECT id,priority FROM tickets WHERE id >= 50000 ORDER BY id')
            run('commit','COMMIT')
            run('begin_rollback','BEGIN')
            run('rollback_write','UPDATE tickets SET priority = 999 WHERE id = 2 RETURNING id,priority')
            run('rollback','ROLLBACK')
            run('rollback_read','SELECT id,priority FROM tickets WHERE id = 2')
    return {'complete':True,'cases':cases,'as_of_reference':'PostgreSQL frozen table copy at seed version; equivalent row contents, not native PostgreSQL AS OF syntax','prepared_reference':'psycopg prepare=True, persistent connection; Lattice cached logical plan / AST with parameter rebind','C1_effort_days':None,'C1_budget_status':'unresolved'}

def benchmark(binary,candidate,root,output,dsn,rows):
    import psycopg
    results=[]
    with psycopg.connect(dsn, autocommit=True) as pg:
        pg.execute('DROP TABLE IF EXISTS tickets, users, tickets_at_seed')
        pg.execute('CREATE TABLE users(id bigint PRIMARY KEY, name text NOT NULL)')
        pg.execute('CREATE TABLE tickets(id bigint PRIMARY KEY, owner bigint NOT NULL, priority bigint NOT NULL)')
        pg.execute("INSERT INTO users SELECT i, 'user'||lpad(i::text,3,'0') FROM generate_series(0,99) i")
        pg.execute('INSERT INTO tickets SELECT i,mod(i,100),mod(i,7) FROM generate_series(0,%s) i',(rows-1,))
        pg.execute('ANALYZE')
        pg.execute('SET max_parallel_workers_per_gather=0')
        pg.execute('SET jit=off')
        pg.execute('SET synchronous_commit=on')
        config=dict(pg.execute("SELECT name,setting FROM pg_settings WHERE name IN ('fsync','synchronous_commit','full_page_writes','wal_sync_method','shared_buffers','max_parallel_workers_per_gather','server_version','jit')").fetchall())
        if config['fsync']!='on' or config['synchronous_commit']!='on':raise RuntimeError('PostgreSQL durability weakened')
        with Host(binary,candidate,root,rows,output/f'{candidate}-host.stderr') as host:
            assert not host.ready['fault_hooks'],'performance binary must not compile fault hooks'
            for name,sql in QUERIES.items():
                expected=[list(row) for row in pg.execute(sql,prepare=False).fetchall()]
                for _ in range(2):
                    actual,_,_=host.execute(sql);assert same_rows(actual,expected),(candidate,name,'warmup mismatch')
                    assert same_rows([list(r) for r in pg.execute(sql,prepare=False).fetchall()],expected)
                client,engine,postgres=[],[],[]
                # Alternate acquisition order to avoid always giving one candidate the warmest cache.
                for n in range(9):
                    def lattice():
                        actual,outer,inner=host.execute(sql);assert same_rows(actual,expected),(candidate,name,'result mismatch');client.append(outer);engine.append(inner)
                    def reference():
                        start=time.perf_counter_ns();actual=[list(r) for r in pg.execute(sql,prepare=False).fetchall()];postgres.append(time.perf_counter_ns()-start);assert same_rows(actual,expected)
                    if n%2:reference();lattice()
                    else:lattice();reference()
                ratio=statistics.median(client)/statistics.median(postgres)
                results.append({'query':name,'sql':sql,'rows':len(expected),'lattice_client':summary(client),'lattice_engine_parse_execute':summary(engine),'postgres_client':summary(postgres),'client_ratio':ratio,'C4_within_3x_pg':ratio<=3})
    return {'postgres_config':config,'lattice_ready':host.ready,'queries':results,'transport':{'postgres':'persistent psycopg/libpq over TCP localhost, including server execution, wire transfer, decoding and Python row-list materialization','lattice':'persistent child JSON pipe including request serialization, engine execution, serde JSON response, pipe transfer, Python native scalar JSON decoding (json-scalars-v1); exact cell-type/value oracle','engine_ns':'separate in-process parse/plan/execute; not compared to PG server-only execution','planning':'C4 ad-hoc queries: PostgreSQL prepare=False; Lattice prepared=false; DataFusion creates snapshot context/provider each statement and includes batch conversion cost','caveat':'protocols and codecs differ; full client numbers are reported alongside, not mislabeled as equal pure executor cost'},'durability':'PostgreSQL fsync+synchronous_commit+full_page_writes; Lattice StdIo fsync WAL commit, no checkpoint/heap/fsync group commit; not equivalent storage designs','phase1_baseline_comparison':'not established: no comparable Phase1 SQL workload baseline'}

def kill_case(binary,candidate,root,output,seed):
    host=Host(binary,candidate,root,2000,output/f'{candidate}-kill-{seed}.stderr')
    observed={'submitted':0,'acked':[],'seed':seed}
    assert host.ready['fault_hooks'],'kill test requires separately built fault-hooks feature'
    try:
        target=2+seed%3
        for n in range(32):
            host.submit('UPDATE tickets SET priority = priority + 1 WHERE id >= 0 RETURNING id, priority',commit_barrier=n if n==target else None)
            observed['submitted']+=1
        while True:
            reply=host.receive()
            if 'before_commit' in reply:
                assert reply['before_commit']==target and reply['staged_rows']==2000,reply
                assert len(observed['acked'])==target
                observed['inside_statement_before_commit']=reply
                break
            if 'Err' in reply['result']:raise RuntimeError(reply['result']['Err'])
            observed['acked'].append({'n':len(observed['acked']),'lsn':reply['lsn']})
        time.sleep((seed*7919%1000)/1e6)
        assert host.proc.poll() is None,'writer exited before kill'
        host.kill_group();host.proc.wait(timeout=10)
        assert host.proc.returncode!=0,'kill reported clean exit'
        host.reader.join(timeout=10)
        assert not host.reader.is_alive(),'reader did not reach EOF'
        while not host.events.empty():
            reply=host.events.get()
            if reply is None:continue
            if 'unpublished_tail' in reply:
                observed['unpublished_tail']=reply;continue
            if 'protocol_error' in reply:
                raise RuntimeError(reply)
            if 'Err' in reply['result']:raise RuntimeError(reply['result']['Err'])
            observed['acked'].append({'n':len(observed['acked']),'lsn':reply['lsn']})
    finally:
        host.close(kill=True)
        save(output/f'{candidate}-kill-{seed}-observed.json',observed)
    verify=subprocess.run([str(binary),'--mode','verify-kill','--dir',str(root),'--rows','2000','--observed',str(output/f'{candidate}-kill-{seed}-observed.json')],text=True,capture_output=True,timeout=60)
    (output/f'{candidate}-kill-{seed}-verify.stdout').write_text(verify.stdout)
    (output/f'{candidate}-kill-{seed}-verify.stderr').write_text(verify.stderr)
    if verify.returncode:raise RuntimeError(f'kill recovery verification failed: {verify.returncode}')
    return json.loads(verify.stdout)

def deny_command(features):
    args=['cargo','deny','--manifest-path',str(MANIFEST),'--config',str(HERE.parent.parent/'deny.toml')]
    if features:args+=['--features',','.join(features)]
    return args+['check']

def preflight(dsn,output):
    import psycopg
    checks=[]
    for candidate,features in FEATURES.items():
        check=command(deny_command(features)+['--help'],output/f'{candidate}-deny-cli-preflight.log',timeout=30)
        checks.append(check)
        if check['exit']!=0:raise RuntimeError(f'cargo deny CLI preflight failed for {candidate}')
    with psycopg.connect(dsn,autocommit=True) as pg:
        pg.execute('BEGIN')
        try:
            pg.execute('CREATE TEMP TABLE tickets(id bigint,owner bigint,priority bigint) ON COMMIT DROP')
            pg.execute('INSERT INTO tickets SELECT i,mod(i,100),mod(i,7) FROM generate_series(0,%s) i',(6,))
            assert pg.execute('SELECT id,owner,priority FROM tickets ORDER BY id').fetchall()==[(n,n%100,n%7)for n in range(7)]
        finally:pg.execute('ROLLBACK')
    return {'complete':True,'deny_cli':checks,'postgres_parameterized_seed':True}

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--output',type=pathlib.Path,required=True);parser.add_argument('--target-root',type=pathlib.Path,required=True);parser.add_argument('--dsn',required=True);parser.add_argument('--functional-retry',action='store_true');args=parser.parse_args()
    out=args.output.resolve();out.mkdir(parents=True,exist_ok=True);target=args.target_root.resolve();target.mkdir(parents=True,exist_ok=True)
    if not args.functional_retry and any(target.iterdir()):raise RuntimeError('clean build requires an empty target root')
    report={'schema_version':3,'result_codec':RESULT_CODEC,'measurement_mode':'functional_retry' if args.functional_retry else 'clean_build','clean_build_evidence':not args.functional_retry,'functional_cache_hit':os.environ.get('FUNCTIONAL_CACHE_HIT') if args.functional_retry else None,'ADR_status':'proposed','criteria':{'C1':{'effort_days':None,'budget':None,'status':'unresolved: missing ADR budget and no retrospective effort estimate'},'C2':{'source_audit':'required; matching observable traces does not prove no randomized hashing'},'C7':{'status':'written hypothesis in README; full-subset feasibility remains unproven'}},'runner':{'platform':platform.platform(),'python':sys.version,'cpu_count':os.cpu_count()},'candidates':{},'failures':[]}
    data_root=pathlib.Path(tempfile.mkdtemp(prefix='lattice-planner-run-data-'))
    try:
        inputs=[p for p in HERE.rglob('*') if p.is_file() and not {'target','functional-target','__pycache__'}.intersection(p.parts)]
        report['input_sha256']={str(p.relative_to(HERE)):hashlib.sha256(p.read_bytes()).hexdigest()for p in inputs}
        report['revision']=subprocess.check_output(['git','rev-parse','HEAD'],cwd=HERE,text=True).strip()
        report['status_before']=subprocess.check_output(['git','status','--porcelain'],cwd=HERE,text=True)
        report['preflight']=preflight(args.dsn,out)
        save(out/'report.json',report)
        # Registry prefetch is separate; clean mode alone uses fresh compiler targets.
        fetch=command(['cargo','fetch','--locked','--manifest-path',str(MANIFEST)],out/'fetch.log');report['registry_prefetch']=fetch
        if fetch['exit']!=0:raise RuntimeError('registry prefetch failed; compiler timing would include uncontrolled network work')
        # Catch shared bridge/API/test compilation failures before paying for all three backends.
        # This separate check target never warms a candidate's clean compiler target or C5 timing.
        report['shared_compile_preflight']=command(['cargo','check','--locked','--all-targets',
            '--manifest-path',str(MANIFEST),'--target-dir',str(target/'preflight')],out/'shared-compile-preflight.log')
        if report['shared_compile_preflight']['exit']!=0:
            raise RuntimeError('shared experiment compile preflight failed; no candidate measurement attempted')
        baseline_manifest=HERE/'bridge-baseline/Cargo.toml'
        if args.functional_retry:
            baseline=None
            report['bridge_build']={'status':'not measured in functional retry'}
        else:
            report['bridge_fetch']=command(['cargo','fetch','--locked','--manifest-path',str(baseline_manifest)],out/'bridge-fetch.log')
            if report['bridge_fetch']['exit']!=0:raise RuntimeError('bridge registry prefetch failed')
            baseline=command(['cargo','build','--locked','--release','--timings','--manifest-path',str(baseline_manifest),'--target-dir',str(target/'bridge')],out/'bridge-build.log');report['bridge_build']=baseline
        for candidate,features in FEATURES.items():
            row={'features':features};report['candidates'][candidate]=row
            candidate_target=target if args.functional_retry else target/candidate
            try:
                build=['cargo','build','--locked','--release','--timings','--manifest-path',str(MANIFEST),'--target-dir',str(candidate_target)]
                if features:build+=['--features',','.join(features)]
                row['deny']=command(deny_command(features),out/f'{candidate}-deny.log')
                row['C5_dependency_review']=row['deny']['exit']==0
                if not row['C5_dependency_review']:report['failures'].append({'candidate':candidate,'stage':'C5_dependency_review','error':'cargo deny failed; raw findings retained, no policy exception'})
                row['deny_graph']='exact candidate feature graph; no blanket exclusions'
                build_key='functional_build' if args.functional_retry else 'clean_build'
                row[build_key]=command(build,out/f'{candidate}-build.log')
                if row[build_key]['exit']!=0:raise RuntimeError('candidate build failed (raw log retained)')
                if args.functional_retry:
                    row['C5_build_status']='not measured: target may be restored/cached'
                else:
                    row['added_clean_build_s_vs_bridge']=row['clean_build']['elapsed_s']-baseline['elapsed_s'] if baseline['exit']==0 else None
                    row['C5_added_build_under_60s']=row['added_clean_build_s_vs_bridge'] is not None and row['added_clean_build_s_vs_bridge']<60
                tests=['cargo','test','--locked','--release','--manifest-path',str(MANIFEST),'--target-dir',str(candidate_target),'--lib','--test','conformance']
                if features:tests+=['--features',','.join(features)]
                row['conformance']=command(tests,out/f'{candidate}-conformance.log')
                if row['conformance']['exit']!=0:
                    report['failures'].append({'candidate':candidate,'stage':'conformance','error':'candidate regression tests failed; raw log retained'})
                timing=candidate_target/'cargo-timings'
                if timing.exists():shutil.copytree(timing,out/f'{candidate}-cargo-timings',dirs_exist_ok=True)
                tree=['cargo','tree','--locked','--manifest-path',str(MANIFEST),'--edges','normal','--prefix','none']
                if features:tree+=['--features',','.join(features)]
                metadata=['cargo','metadata','--locked','--format-version','1','--manifest-path',str(MANIFEST)]
                if features:metadata+=['--features',','.join(features)]
                # Metadata stdout is machine-readable; keep stderr separate from JSON.
                with (out/f'{candidate}-metadata.json').open('w') as metadata_out, (out/f'{candidate}-metadata.stderr').open('w') as metadata_err:
                    m=subprocess.run(metadata,cwd=HERE,stdout=metadata_out,stderr=metadata_err,timeout=120)
                row['source_audit']=source_hints(out/f'{candidate}-metadata.json',out/f'{candidate}-nondeterminism-hints.json') if m.returncode==0 else {'status':'metadata failed'}
                row['dependency_tree']=command(tree,out/f'{candidate}-tree.txt')
                if m.returncode==0:
                    graph=json.loads((out/f'{candidate}-metadata.json').read_text())
                    row['normal_crate_count_including_root']=len(reachable_packages(graph))
                    row['normal_and_build_crate_count_including_root']=len(reachable_packages(graph,(None,'build')))
                    row['crate_count_source']='resolved Cargo package IDs, not colorized tree lines'
                binary=candidate_target/'release/lattice-sql-planner-spike'
                sims=[]
                for repeat in range(2):
                    path=out/f'{candidate}-sim-{repeat}.json';run=command([str(binary),'--candidate',candidate,'--mode','sim','--seed','42','--output',str(path)],out/f'{candidate}-sim-{repeat}.log',timeout=60);sims.append(run)
                row['sim_runs']=sims
                if all(r['exit']==0 for r in sims):
                    a=json.loads((out/f'{candidate}-sim-0.json').read_text());b=json.loads((out/f'{candidate}-sim-1.json').read_text());row['C2_raw_trace_repeat_equal']=a['run']['trace']==b['run']['trace'];row['representatives_complete_without_runtime']=a['run']['positive_cases_complete'];row['C6_negative_fixtures_match']=a['run']['C6_negative_fixtures_match'];row['C2_status']='execution failed without runtime' if not a['run']['positive_cases_complete'] else 'observable execution/trace checks only; randomized-hash source audit unverified'
                stages=[('representative_differential',lambda:representatives(binary,candidate,data_root/f'{candidate}-representatives',out,args.dsn,20000)),('postgres_comparison',lambda:benchmark(binary,candidate,data_root/f'{candidate}-data',out,args.dsn,20000))]
                for stage,operation in stages:
                    try:row[stage]=operation()
                    except BaseException as error:
                        row[stage]={'incomplete':repr(error),'traceback':traceback.format_exc()}
                        report['failures'].append({'candidate':candidate,'stage':stage,'error':repr(error)})
                    save(out/'report.json',report)
                # Build test-only barriers after all performance work. This reuses a warm target
                # and is excluded from clean build / dependency weight / query timing evidence.
                hook_features=features+['fault-hooks']
                hook_build=['cargo','build','--locked','--release','--manifest-path',str(MANIFEST),'--target-dir',str(candidate_target),'--features',','.join(hook_features)]
                row['fault_hook_build_excluded_from_C5']=command(hook_build,out/f'{candidate}-fault-hooks-build.log')
                if row['fault_hook_build_excluded_from_C5']['exit']!=0:
                    report['failures'].append({'candidate':candidate,'stage':'fault_hook_build','error':'test-only hook build failed'})
                else:
                    for seed in range(3):
                        try:row[f'process_kill_{seed}']=kill_case(binary,candidate,data_root/f'{candidate}-kill-{seed}',out,seed)
                        except BaseException as error:
                            row[f'process_kill_{seed}']={'incomplete':repr(error),'traceback':traceback.format_exc()}
                            report['failures'].append({'candidate':candidate,'stage':f'process_kill_{seed}','error':repr(error)})
                        save(out/'report.json',report)
            except BaseException as error:
                row['incomplete']=repr(error);row['traceback']=traceback.format_exc();report['failures'].append({'candidate':candidate,'error':repr(error)})
            finally:save(out/'report.json',report)
        report['status_after']=subprocess.check_output(['git','status','--porcelain'],cwd=HERE,text=True)
        report['input_hashes_unchanged']=all(hashlib.sha256((HERE/name).read_bytes()).hexdigest()==digest for name,digest in report['input_sha256'].items())
        report['source_code_provenance']='revision and hashed experiment files; dirty production content not vouched for'
        report['source_status_unchanged']=report['status_before']==report['status_after']
        if not report['source_status_unchanged'] or not report['input_hashes_unchanged']:
            report['failures'].append({'stage':'source_freshness','error':'source status or hashed experiment input changed during measurement'})
        report['measurements_complete']=not report['failures']
        report['criterion_passes_are_not_phase_readiness']=True
    except BaseException as error:
        report['failures'].append({'coordinator':repr(error),'traceback':traceback.format_exc()});report['measurements_complete']=False
    finally:
        report['assessment']=assess(report)
        save(out/'criteria.json',report['assessment'])
        save(out/'report.json',report)
        shutil.rmtree(data_root,ignore_errors=True)
    return 0 if report.get('measurements_complete') else 1
if __name__=='__main__':raise SystemExit(main())
