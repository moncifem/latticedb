"""Conservative ADR0013 assessment: measurements never automatically accept the ADR."""
import statistics

QUERIES = {'point', 'range_10000', 'join', 'group'}


def assess(report):
    result = {}
    for name in report.get('candidate_scope', ('custom', 'parser', 'datafusion')):
        row = report.get('candidates', {}).get(name, {})
        queries = row.get('postgres_comparison', {}).get('queries', [])
        measured = {query.get('query'): query for query in queries}
        ratios = {}
        if len(queries) == 4 and set(measured) == QUERIES:
            for key, query in measured.items():
                actual = query.get('lattice_client', {}).get('samples_ns', [])
                reference = query.get('postgres_client', {}).get('samples_ns', [])
                if len(actual) == len(reference) == 9 and all(
                        type(n) in (int, float) and 0 < n < float('inf')
                        for n in actual + reference):
                    ratios[key] = statistics.median(actual) / statistics.median(reference)
        pg = ('failed' if any(ratio > 3 for ratio in ratios.values()) else
              'passed' if set(ratios) == QUERIES else 'missing')
        kill = all(row.get(f'process_kill_{seed}', {}).get('complete') is True
                   and row.get(f'process_kill_{seed}', {}).get('terminal_reopen') is True
                   for seed in range(3))
        clean = report.get('measurement_mode') == 'clean_build'
        build = row.get('C5_added_build_under_60s') if clean else None
        deny = row.get('C5_dependency_review')
        result[name] = {
            'C1': {'status': 'unresolved', 'budget_days': None, 'historical_effort_days': None,
                   'coverage': 'five representatives; not full declared subset'},
            'C2': {'status': 'failed' if name == 'datafusion' else 'qualified',
                   'source_review': 'C2_AUDIT.md: pinned DataFusion reaches host clocks and randomized hashing; other paths bounded',
                   'repeat_trace_equal': row.get('C2_raw_trace_repeat_equal'),
                   'implemented_cases_without_runtime': row.get('representatives_complete_without_runtime')},
            'C3': {'status': 'bounded process tests passed' if kill else 'missing or failed',
                   'limit': 'precommit process interruption; no hardware power-loss or WAL-interior interruption proof'},
            'C4': {'status': 'failed' if pg == 'failed' else 'unresolved',
                   'same_runner_postgres_3x': pg, 'recomputed_median_client_ratios': ratios,
                   'phase1_regression': 'unproven: no comparable SQL baseline'},
            'C5': {'status': 'failed' if build is False or deny is False else 'unresolved',
                   'added_clean_build_under_60s': build, 'dependency_policy': deny,
                   'engine_runtime': 'production graph isolated; experiment runtime is not an engine dependency'},
            'C6': {'status': 'bounded fixtures passed' if row.get('C6_negative_fixtures_match') is True else 'missing or failed',
                   'limit': 'negative.slt prototype fixtures; not full subset conformance'},
            'C7': {'status': 'qualified written assessment',
                   'evidence': 'README.md: correlated binding and window operators not implemented; no no-rewrite proof'},
        }
    if report.get('measurement_mode') == 'semantic_confirmation':
        for row in result.values():
            row['C4']={'status':'not measured in semantic_confirmation','same_runner_postgres_3x':'not measured',
                       'recomputed_median_client_ratios':{},'phase1_regression':'not measured'}
            row['C5']={'status':'not measured in semantic_confirmation','added_clean_build_under_60s':None,
                       'dependency_policy':None,'engine_runtime':'not measured'}
    return {'ADR_status': 'proposed', 'automatic_acceptance': False, 'candidates': result}
