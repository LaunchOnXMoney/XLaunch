"""Isolated browser/API test using real Pinata uploads; no Solana transaction is sent."""
import json,os,socket,subprocess,time
from pathlib import Path
from urllib.request import Request,urlopen
from urllib.error import HTTPError,URLError
from PIL import Image

project=Path(__file__).resolve().parents[1]
root=project/'var/web-e2e'
root.mkdir(parents=True,exist_ok=True,mode=0o700)
inbox_path=root/'inbox.json'
# A new isolated directory permits rerunning without touching the real receiver.
run=root/str(time.time_ns());run.mkdir(mode=0o700)
reports=run/'reports';reports.mkdir(mode=0o700)
report_path=reports/'e2e.json'
inbox_path=run/'inbox.json'
records=[]
def save_inbox():
    tmp=inbox_path.with_suffix('.tmp');tmp.write_text(json.dumps({'version':1,'next_sequence':len(records)+1,'transactions':records}));tmp.replace(inbox_path)
def add(amount,memo):
    records.append({'sequence':len(records)+1,'received_at':time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime()),'payment':{'sender':'@web-e2e','amount':amount,'memo':memo}});save_inbox()
save_inbox()
env=os.environ.copy()
for name in ['PINATA_JWT','IPFS_GATEWAY']:
    if not env.get(name):
        raise SystemExit(f'{name} must be set in the environment for real Pinata tests')
with socket.socket() as sock:
    sock.bind(('127.0.0.1',0));port=sock.getsockname()[1]
endpoint=f'http://127.0.0.1:{port}'
log=(run/'server.log').open('w+')
proc=None

def get(path):
    with urlopen(endpoint+path,timeout=10) as r:return json.load(r)
def start():
    global proc
    proc=subprocess.Popen([str(project/'target/debug/launchpad'),f'127.0.0.1:{port}',str(inbox_path),str(run/'data'),str(project/'frontend'),str(project/'frontend-api'),str(project/'frontend-vendor')],env=env,stdout=log,stderr=log)
    until=time.monotonic()+15
    while time.monotonic()<until:
        assert proc.poll() is None,'backend exited during startup'
        try:
            if get('/healthz')['ok']:return
        except (URLError,ConnectionError):time.sleep(.05)
    raise AssertionError('backend did not become healthy')
def wait_for(fn,seconds=60):
    until=time.monotonic()+seconds
    while time.monotonic()<until:
        value=fn()
        if value:return value
        time.sleep(.2)
    raise AssertionError('condition timed out')
try:
    start()
    image_path=run/'test.png';Image.new('RGB',(16,16),'#1d9bf0').save(image_path)
    req=Request(endpoint+'/api/images',data=image_path.read_bytes(),headers={'Content-Type':'image/png'})
    with urlopen(req,timeout=60) as r:upload=json.load(r)
    assert upload['uri']=='ipfs://'+upload['cid']
    with urlopen(Request(endpoint+'/api/images',data=image_path.read_bytes(),headers={'Content-Type':'image/png'}),timeout=60) as r:assert json.load(r)==upload
    # Content is served as the exact uploaded bytes from the memory cache.
    with urlopen(endpoint+upload['preview_url']) as r:assert r.read()==image_path.read_bytes()
    config={'name':'XLaunch Integration Test','symbol':'XLTEST','image_uri':upload['uri'],'socials':{'website':'https://example.com','twitter':'https://x.com/example','telegram':'https://t.me/example'}}
    def prepare(value):
        with urlopen(Request(endpoint+'/api/launch-config/prepare',data=json.dumps(value).encode(),headers={'Content-Type':'application/json'}),timeout=60) as r:return json.load(r)
    prepared=prepare(config)
    assert prepare(config)==prepared,'metadata preparation must be idempotent'
    assert prepared['uri']!=upload['uri'],'memo must reference metadata, not the raw image'
    note='Name: XLaunch Integration Test\nSymbol: XLTEST\nMetadata Uri: '+prepared['uri']
    assert len(note.splitlines())==3
    # A crash after Copy config but before receiving payment must preserve the URI mapping.
    proc.kill();assert proc.wait(timeout=5)==-9
    start()
    assert get('/api/config')['launch_fee_cents']==100
    wait_for(lambda:get('/api/health')['market']['curve_config_slot'])
    add('1.00',note)
    token=wait_for(lambda:(get('/api/tokens?status=raising&sort=recent')['tokens'] or [None])[0])
    mint=token['mint'];assert not mint.startswith('0x')
    add('125.50','So11111111111111111111111111111111111111112'+'  \t'+mint)
    token=wait_for(lambda:(lambda t:t if t['raised_cents']==12550 and t['metadata_uri'] else None)(get('/api/tokens/'+mint)['token']))
    contribution=get('/api/tokens/'+mint)['contributions'][0]
    assert contribution['wallet']=='So11111111111111111111111111111111111111112'
    assert contribution['accepted_cents']==12550
    assert token['market_cap_usd']>125.50
    assert token['market_cap_source']=='presale_curve'
    assert token['metadata_uri']==prepared['uri'],'must reuse the exact metadata URI in the memo'
    assert token['image_uri']==upload['uri']
    assert token['image_url']==upload['preview_url']
    with urlopen(endpoint+token['image_url']) as r:
        assert r.read()==image_path.read_bytes()
        assert 'immutable' in r.headers['Cache-Control']
    assert token['socials']==config['socials']
    metadata=get('/api/tokens/'+mint+'/metadata')
    assert metadata['twitter']=='https://x.com/example'
    assert metadata['telegram']=='https://t.me/example'
    assert metadata['external_url']=='https://example.com'
    assert metadata['image']==upload['uri']
    # Read back the exact metadata JSON from IPFS, independent of our API.
    cid=token['metadata_uri'][7:]
    gateway=env['IPFS_GATEWAY']
    with urlopen(Request(gateway+cid,headers={'User-Agent':'XLaunch-Metadata-Verification/1.0'}),timeout=60) as r:pinned=json.load(r)
    assert pinned==metadata,'IPFS readback mismatch'
    assert 'mint_keypair' not in json.dumps(get('/api/tokens/'+mint))
    for path in ['/var/launchpad/pinata.env','/catalog.json','/pins.json']:
        try:urlopen(endpoint+path);raise AssertionError('private file exposed')
        except HTTPError as e:assert e.code==404
    first=Request(endpoint+'/api/tokens?status=raising&sort=recent')
    with urlopen(first) as r:etag=r.headers['ETag'];r.read()
    try:urlopen(Request(first.full_url,headers={'If-None-Match':etag}));raise AssertionError('ETag ignored')
    except HTTPError as e:assert e.code==304
    before=get('/api/health')
    for _ in range(100):get('/api/tokens?status=raising&sort=recent')
    after=get('/api/health')
    assert after['market']['price_requests']==before['market']['price_requests']
    assert after['market']['supply_requests']==before['market']['supply_requests']
    assert after['source_reads']==before['source_reads'],'read endpoint reread source file'
    assert after['cache_hits']>=before['cache_hits']+100
    search=get('/api/tokens?status=raising&sort=recent&q='+mint)
    assert search['total']==1 and search['tokens'][0]['mint']==mint
    public_before=get('/api/tokens/'+mint)
    private_before=json.loads((run/'data/catalog.json').read_text())
    proc.kill();assert proc.wait(timeout=5)==-9
    start()
    assert get('/api/tokens/'+mint)==public_before
    assert json.loads((run/'data/catalog.json').read_text())==private_before
    browser_env=os.environ.copy();browser_env.update({'WEB_TEST_URL':endpoint,'WEB_TEST_MINT':mint,'WEB_TEST_IMAGE':str(image_path),'WEB_TEST_OUT':str(reports)})
    subprocess.run(['node',str(project/'tools/test_web_browser.cjs')],env=browser_env,check=True,timeout=90)
    browser=json.loads((reports/'browser.json').read_text())
    # Match the observed transport: original newlines arrived as multiple spaces.
    add('1.00',browser['copied_note'].replace('\n','        '))
    browser_token=wait_for(lambda:(get('/api/tokens?status=raising&sort=recent&q=SOCIAL')['tokens'] or [None])[0])
    assert browser_token['socials']==config['socials']
    assert browser_token['metadata_uri']==browser['copied_note'].split('Metadata Uri: ',1)[1]
    assert browser_token['image_uri']==upload['uri']
    assert browser_token['image_url']==upload['preview_url']
    report={'status':'passed','memo_lines':3,'browser_note_ingestion':'passed','prepared_uri_survives_sigkill_before_payment':True,'same_metadata_uri_used_for_launch':True,'image_ipfs_uri':upload['uri'],'metadata_ipfs_uri':token['metadata_uri'],'metadata_readback_matches':True,'socials_saved_and_returned':True,'reversed_buy_memo_accepted':True,'raising_cents':12550,'cap_cents':token['cap_cents'],'cached_requests_without_source_reads':100,'etag_status':304,'sigkill_recovery':'passed','private_paths_status':404,'fixture_directory':str(run)}
    report_path.write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report))
except BaseException:
    log.flush();log.seek(0);print(log.read());raise
finally:
    if proc is not None and proc.poll() is None:proc.kill();proc.wait(timeout=5)
    log.close()
