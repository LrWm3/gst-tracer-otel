from pathlib import Path
import json, os, subprocess, time
root = Path('/work/target/profile-soak-review')
root.mkdir(exist_ok=True)
def binary(stem):
    candidates = [p for p in Path('/work/target/debug/deps').glob(stem+'-*') if p.is_file() and os.access(p,os.X_OK) and p.suffix!='.so']
    assert len(candidates)==1,candidates
    return str(candidates[0])
unit, integration = binary('gstoteltracer'), binary('profiles')
checks = [(unit, 'profiles::linux::tests::'+s) for s in ['capture_context_edges_thread_retirement_and_shutdown_are_isolated', 'file_symbolization_progresses_while_dynamic_loader_is_locked', 'full_or_failed_upload_queue_never_blocks_processing', 'concurrent_ring_transfers_complete_samples_in_order']]
checks += [(integration, s) for s in ['slow_profile_receiver_does_not_extend_shutdown_deadline','profiles_only_shutdown_keeps_its_deadline_with_a_slow_receiver']]
results=[]
with (root/'stress.log').open('w') as log:
    for repeat in range(10):
        for exe, check in checks:
            start=time.monotonic()
            run=subprocess.run([exe, '--exact', check, '--nocapture'], capture_output=True, text=True, timeout=45)
            log.write(run.stdout+run.stderr);log.flush()
            assert run.returncode==0 and '1 passed;' in run.stdout,run.stdout+run.stderr
            results.append(dict(repeat=repeat, test=check, seconds=time.monotonic()-start))
        print(f'PASS round {repeat+1}/10 ({len(results)} tests)',flush=True)
(root/'stress.json').write_text(json.dumps(results,indent=2)+'\n')
