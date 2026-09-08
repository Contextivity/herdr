import json, os, shutil, socket, subprocess, tempfile, time, threading
from pathlib import Path
import argparse
parser=argparse.ArgumentParser(description='Isolated native startup transport and cleanup regression')
parser.add_argument('--binary', required=True, type=Path)
BINARY=parser.parse_args().binary.resolve()
def poll(predicate,seconds=10):
    end=time.monotonic()+seconds
    while time.monotonic()<end:
        result=predicate()
        if result:return result
        time.sleep(.03)
    raise AssertionError('bounded condition timed out')
with tempfile.TemporaryDirectory(prefix='herdr-ticket-') as tmp:
    # macOS aliases /tmp to /private/tmp; compare one physical cwd throughout.
    root=Path(tmp).resolve(); env={k:v for k,v in os.environ.items() if not k.startswith('HERDR_')}
    env.update(XDG_CONFIG_HOME=str(root/'config'),XDG_STATE_HOME=str(root/'state'),
        HERDR_SOCKET_PATH=str(root/'server.sock'),HERDR_CLIENT_SOCKET_PATH=str(root/'client.sock'),
        HERDR_CONFIG_PATH=str(root/'config.toml'),ZDOTDIR=str(root),TMPDIR=str(root),SHELL=shutil.which('zsh'))
    (root/'config.toml').write_text('[terminal]\ndefault_shell = "'+shutil.which('zsh')+'"\n[update]\nversion_check = false\n')
    (root/'record.py').write_text('import os,sys,json\nwith open(os.environ["TEST_RESULT"],"a") as f: f.write(json.dumps(dict(argv=sys.argv[1:], preparation=os.environ.get("PREPARED")))+"\\n")\n')
    (root/'.zshrc').write_text('zmodload zsh/datetime\nif [[ -n "$TEST_MARKER" ]]; then\n print begin > "$TEST_MARKER"\n launch_begin=$EPOCHREALTIME\n while (( EPOCHREALTIME - launch_begin < ${TEST_DELAY:-2} )); do :; done\n print ready >> "$TEST_MARKER"\nfi\nPS1="READY> "\ncodex() { python3 '+str(root/'record.py')+' "$@"; }\nprint ready > '+str(root)+'/ready-$$\n')
    def api(method,params):
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(12);sock.connect(str(root/'server.sock'))
            sock.sendall((json.dumps(dict(id='isolated',method=method,params=params))+'\n').encode())
            with sock.makefile('rb') as f:return json.loads(f.readline())
    with open(root/'server.log','w') as log:
        server=subprocess.Popen([str(BINARY),'server'],env=env,stdout=log,stderr=log)
        try:
            poll(lambda:(root/'server.sock').exists())
            rows=[]
            sentinel=api('workspace.create',dict(cwd=str(root),label='unrelated-sentinel'))['result']
            sentinel_pane=sentinel['root_pane']['pane_id']
            def assert_sentinel():
                assert 'error' not in api('pane.get',dict(pane_id=sentinel_pane))
            for size in [1000,8000]:
                marker=root/f'marker-{size}';result=root/f'result-{size}'
                workspace=api('workspace.create',dict(cwd=str(root),label=f'ticket-{size}',env=dict(TEST_MARKER=str(marker),TEST_RESULT=str(result))))
                pane=workspace['result']['root_pane']['pane_id']
                poll(marker.exists)
                argument=('x' * size)+" ' quoted $literal /space path"
                prep=('y'*size)+" ' prepared $literal"
                import shlex
                prepared=api('agent.startup',dict(operation='prepare',start=dict(name=f'test-{size}',kind='codex',pane_id=pane,args=['--',argument],timeout_ms=6000),preparation=['export PREPARED='+shlex.quote(prep)]))
                assert 'error' not in prepared,prepared
                receipt=prepared['result']['receipt']
                observed=api('agent.startup',dict(operation='inspect',receipt=receipt))
                assert observed.get('result')==dict(type='agent_startup',receipt=receipt,state='prepared'),observed
                assert 'ready' not in marker.read_text(),'test did not exercise early initialization'
                launched=api('agent.startup',dict(operation='launch',receipt=receipt));assert 'error' not in launched,launched
                again=api('agent.startup',dict(operation='launch',receipt=receipt));assert again['error']['code']=='startup_already_submitted',again
                poll(result.exists)
                poll(lambda:(Path(receipt['ticket'])/'finished').read_text().strip())
                observed=api('agent.startup',dict(operation='inspect',receipt=receipt))
                assert observed.get('result')==dict(type='agent_startup',receipt=receipt,state='finished'),observed
                actual=result.read_text().splitlines()
                assert len(actual)==1,actual
                assert json.loads(actual[0])==dict(argv=['--',argument],preparation=prep)
                # A modified receipt must fail without changing the workspace.
                for key,value in [('terminal_id','reused'),('shell_pid',receipt['shell_pid']+1),('shell_lifetime','reused'),('workspace_id','reused'),('ticket','unknown')]:
                    bad=dict(receipt);bad[key]=value
                    denied=api('agent.startup',dict(operation='inspect',receipt=bad));assert 'error' in denied,denied
                    denied=api('agent.startup',dict(operation='cleanup',receipt=bad));assert 'error' in denied,denied
                cleaned=api('agent.startup',dict(operation='cleanup',receipt=receipt));assert cleaned.get('result',{}).get('state')=='cleaned',cleaned
                missing=api('pane.get',dict(pane_id=pane));assert missing['error']['code']=='pane_not_found',missing
                assert not Path(receipt['ticket']).exists()
                poll(lambda:api('agent.startup',dict(operation='inspect',receipt=receipt)).get('result',{}).get('state')=='closed')
                retired=api('agent.startup',dict(operation='cleanup',receipt=receipt))
                assert retired.get('result',{}).get('state')=='cleaned',retired
                rows.append(dict(payload_bytes=len(argument),early=True,execution_count=1,exact_arguments=True,exact_preparation=True,duplicate_launch_rejected=True,cleanup='cleaned'))
            # Ordinary agent.start must also avoid the canonical PTY input limit.
            # Hold zsh initialization so handoff races the genuinely queued source.
            for size in [1000, 8000]:
                marker=root/f'ordinary-marker-{size}';result=root/f'ordinary-result-{size}'
                workspace=api('workspace.create',dict(cwd=str(root),label=f'ordinary-{size}',
                    env=dict(TEST_MARKER=str(marker),TEST_RESULT=str(result),TEST_DELAY='4')))
                pane=workspace['result']['root_pane']['pane_id'];poll(marker.exists)
                argument=('x'*size)+" ' quoted $literal /space path";reply={}
                def start_ordinary():
                    try: reply['response']=api('agent.start',dict(name=f'ordinary-{size}',kind='codex',
                        pane_id=pane,args=['--',argument],timeout_ms=6000))
                    except Exception as exc: reply['error']=str(exc)
                worker=threading.Thread(target=start_ordinary);worker.start()
                try:
                    poll(lambda:any(a.get('name')==f'ordinary-{size}' for a in api('agent.list',{})['result']['agents']),seconds=2)
                    held=api('server.live_handoff',{})
                    assert held.get('error',{}).get('code')=='handoff_failed',held
                    assert 'ordinary agent startup is queued' in held['error']['message'],held
                    poll(result.exists)
                finally: worker.join(timeout=15)
                assert not worker.is_alive() and 'error' not in reply,reply
                assert 'error' not in reply['response'],reply
                actual=result.read_text().splitlines();assert len(actual)==1,actual
                assert json.loads(actual[0])==dict(argv=['--',argument],preparation=None)
                assert 'error' not in api('workspace.close',dict(workspace_id=workspace['result']['workspace']['workspace_id']))
                assert_sentinel()
                rows.append(dict(case='ordinary-start',payload_bytes=len(argument),execution_count=1,
                    exact_arguments=True,pending_handoff_rejected=True,sentinel_preserved=True))
            # Prepared-but-never-submitted and externally closed tickets must
            # retire their files and preserve sufficient receipt proof for cleanup.
            for external_close in [False, True]:
                workspace=api('workspace.create',dict(cwd=str(root),label='prepared'))
                pane=workspace['result']['root_pane']['pane_id']
                def prepare():
                    response=api('agent.startup',dict(operation='prepare',start=dict(name='prepared',kind='codex',pane_id=pane,args=[],timeout_ms=6000),preparation=[]))
                    if response.get('error',{}).get('code')=='agent_pane_busy':return None
                    assert 'error' not in response,response
                    return response
                receipt=poll(prepare)['result']['receipt']
                poll(lambda:(root/f"ready-{receipt['shell_pid']}").exists())
                if external_close:
                    closed=api('workspace.close',dict(workspace_id=receipt['workspace_id']))
                    assert 'error' not in closed,closed
                    assert not Path(receipt['ticket']).exists(),'terminal close leaked private startup files'
                cleaned=api('agent.startup',dict(operation='cleanup',receipt=receipt))
                if 'error' in cleaned:
                    print(json.dumps(dict(cleanup_failure=cleaned,external_close=external_close,observed=api('pane.process_info',dict(pane_id=pane)))),flush=True)
                assert cleaned.get('result',{}).get('state')=='cleaned',cleaned
                assert not Path(receipt['ticket']).exists()
                retired=api('agent.startup',dict(operation='cleanup',receipt=receipt))
                assert retired.get('result',{}).get('state')=='cleaned',retired
                rows.append(dict(case='already-absent' if external_close else 'never-submitted',cleanup='cleaned',cleanup_retry_acknowledged=True,files_removed=True))
            # The presentation cwd may remain stale after a builtin cd without OSC.
            workspace=api('workspace.create',dict(cwd=str(root),label='live-cwd'))['result']
            pane=workspace['root_pane']['pane_id']
            def prepare_cwd():
                response=api('agent.startup',dict(operation='prepare',start=dict(name='live-cwd',kind='codex',pane_id=pane,args=[],timeout_ms=6000),preparation=[]))
                if response.get('error',{}).get('code')=='agent_pane_busy':return None
                assert 'error' not in response,response
                return response['result']['receipt']
            receipt=poll(prepare_cwd)
            poll(lambda:(root/f"ready-{receipt['shell_pid']}").exists())
            changed=root/'changed-cwd';changed.mkdir()
            marker=root/'live-cwd-marker'
            command="precmd_functions=(); printf '\\033]7;file://localhost"+receipt['cwd']+"\\007'; cd "+shlex.quote(str(changed))+"; pwd > "+shlex.quote(str(marker))
            sent=api('pane.send_input',dict(pane_id=pane,text=command,keys=['Enter']))
            assert 'error' not in sent,sent
            poll(lambda:marker.exists() and marker.read_text().strip()==str(changed.resolve()))
            cached=api('pane.get',dict(pane_id=pane))['result']['pane']['cwd']
            assert cached==receipt['cwd'],('fixture requires stale presentation cwd',cached,receipt)
            denied=api('agent.startup',dict(operation='inspect',receipt=receipt))
            assert denied.get('error',{}).get('code')=='startup_ownership_mismatch',denied
            assert Path(receipt['ticket']).exists()
            assert_sentinel()
            assert 'error' not in api('workspace.close',dict(workspace_id=workspace['workspace']['workspace_id']))
            rows.append(dict(case='live-cwd',unchanged_receipt=True,stale_reported_cwd=True,changed_live_cwd=True,refused=True))
            # Immutable validation rejects before ticket creation; failed preparation
            # under noclobber still acknowledges shell return and permits safe retry.
            workspace=api('workspace.create',dict(cwd=str(root),label='noclobber'))['result']
            pane=workspace['root_pane']['pane_id']
            for timeout in [0, 3000, 300001]:
                rejected=api('agent.startup',dict(operation='prepare',start=dict(name='noclobber',kind='codex',pane_id=pane,args=[],timeout_ms=timeout),preparation=[]))
                assert rejected.get('error',{}).get('code')=='invalid_agent_timeout',rejected
            def prepare_noclobber():
                response=api('agent.startup',dict(operation='prepare',start=dict(name='noclobber',kind='codex',pane_id=pane,args=[],timeout_ms=6000),preparation=['set -C; false']))
                if response.get('error',{}).get('code')=='agent_pane_busy':return None
                assert 'error' not in response,response
                return response['result']['receipt']
            receipt=poll(prepare_noclobber)
            poll(lambda:(root/f"ready-{receipt['shell_pid']}").exists())
            assert 'error' not in api('agent.startup',dict(operation='launch',receipt=receipt))
            poll(lambda:(Path(receipt['ticket'])/'finished').read_text().strip()=='1')
            for _ in range(3):
                cleaned=api('agent.startup',dict(operation='cleanup',receipt=receipt))
                assert cleaned.get('result',{}).get('state')=='cleaned',cleaned
                assert not Path(receipt['ticket']).exists()
            rows.append(dict(case='invalid-timeout-and-noclobber',invalid_timeouts_rejected=3,completion_status=1,cleanup_acknowledgements=3))
            # Keep each original receipt unchanged while mutating observed state.
            for scenario in ['foreground', 'background', 'moved']:
                workspace=api('workspace.create',dict(cwd=str(root),label=scenario))['result']
                pane=workspace['root_pane']['pane_id']
                def prepare_adversary():
                    response=api('agent.startup',dict(operation='prepare',start=dict(name=scenario,kind='codex',pane_id=pane,args=[],timeout_ms=6000),preparation=[]))
                    if response.get('error',{}).get('code')=='agent_pane_busy':return None
                    assert 'error' not in response,response
                    return response['result']['receipt']
                receipt=poll(prepare_adversary)
                poll(lambda:(root/f"ready-{receipt['shell_pid']}").exists())
                owned_workspace=receipt['workspace_id']
                child_pid=None
                if scenario in ['foreground', 'background']:
                    child_file=root/(scenario+'.pid')
                    command="sh -c 'echo $$ > "+str(child_file)+"; exec sleep 60'"+(' &' if scenario=='background' else '')
                    sent=api('pane.send_input',dict(pane_id=pane,text=command,keys=['Enter']))
                    assert 'error' not in sent,sent
                    poll(lambda:child_file.exists() and child_file.read_text().strip())
                    child_pid=int(child_file.read_text())
                    os.kill(child_pid,0)
                elif scenario=='moved':
                    moved=api('pane.move',dict(pane_id=pane,destination=dict(type='new_workspace',label='moved-target')))
                    assert 'error' not in moved,moved
                    # Public pane IDs change with location. Discover by stable terminal identity.
                    listing=api('workspace.list',{})['result']['workspaces']
                    owned_workspace=next(w['workspace_id'] for w in listing if w.get('label')=='moved-target')
                denied=api('agent.startup',dict(operation='cleanup',receipt=receipt))
                assert denied.get('error',{}).get('code')=='startup_ownership_mismatch',(scenario,denied)
                assert Path(receipt['ticket']).exists(),scenario
                assert_sentinel()
                if child_pid is not None:os.kill(child_pid,0)
                # Test-fixture teardown uses its owned workspace, never the refused receipt.
                closed=api('workspace.close',dict(workspace_id=owned_workspace))
                assert 'error' not in closed,closed
                assert not Path(receipt['ticket']).exists()
                if child_pid is not None:
                    def child_gone():
                        try:os.kill(child_pid,0)
                        except ProcessLookupError:return True
                        return False
                    poll(child_gone)
                rows.append(dict(case=scenario,unchanged_receipt=True,refused=True,resource_preserved=True,sentinel_preserved=True))
            assert_sentinel()
            assert 'error' not in api('workspace.close',dict(workspace_id=sentinel['workspace']['workspace_id']))
            print(json.dumps(rows,indent=2),flush=True)
            listing=api('workspace.list',{})
            assert listing['result']['workspaces']==[],listing
            print(json.dumps(dict(final_workspace_count=0,private_launch_directories=len(list(root.glob('herdr-start-*'))))),flush=True)
            assert not list(root.glob('herdr-start-*'))
        finally:
            stopped=subprocess.run([str(BINARY),'server','stop'],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=15)
            try:server.wait(timeout=15)
            except subprocess.TimeoutExpired:server.terminate();server.wait(timeout=5)
            if server.returncode not in (0,-15):print((root/'server.log').read_text()[-3000:])
