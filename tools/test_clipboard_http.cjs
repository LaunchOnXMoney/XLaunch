// Exercise actual copy/paste on an explicitly selected HTTP test site.
const {chromium}=require('playwright');
const fs=require('fs');
const assert=require('assert/strict');
(async()=>{
 for(const name of ['WEB_TEST_URL','WEB_TEST_IMAGE','WEB_TEST_OUT'])assert.ok(process.env[name],name+' must be set');
 fs.mkdirSync(process.env.WEB_TEST_OUT,{recursive:true,mode:0o700});
 const browser=await chromium.launch({headless:true,executablePath:process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH,args:['--no-sandbox']});
 try {
  const page=await browser.newPage();const errors=[];let validations=0;
  page.on('pageerror',e=>errors.push(e.message));
  await page.route('**/api/launch-config/prepare',async route=>{
   validations++;await new Promise(resolve=>setTimeout(resolve,1200));await route.continue();
  });
  await page.goto(new URL('/deploy',process.env.WEB_TEST_URL).href);
  await page.getByText('Send $1 on X Money',{exact:true}).waitFor();
  assert.equal(await page.evaluate(()=>isSecureContext),false);
  await page.getByPlaceholder('Grok Coin',{exact:true}).fill('Clipboard Test');
  await page.getByPlaceholder('GROK',{exact:true}).fill('COPY');
  await page.getByRole('textbox',{name:'X',exact:true}).fill('https://x.com/example');
  await page.locator('input[type=file]').setInputFiles(process.env.WEB_TEST_IMAGE);
  await page.waitForFunction(()=>Array.from(document.querySelectorAll('button')).some(b=>b.textContent==='Copy config'&&!b.disabled));
  await page.evaluate(()=>{
   let inClick=false;
   // Microtasks can run between DOM listeners; the browser task boundary is
   // what distinguishes direct copying from awaiting a network response.
   document.addEventListener('click',()=>{inClick=true;setTimeout(()=>inClick=false,0);},true);
   const copy=window.xlaunchCopy;
   window.xlaunchCopy=text=>{window.copyInClick=inClick;return copy(text);};
  });
  const before=validations;
  await page.getByRole('button',{name:'Copy config',exact:true}).click();
  await page.getByRole('button',{name:'Copied',exact:true}).waitFor();
  assert.equal(await page.evaluate(()=>window.copyInClick),true);
  assert.equal(validations,before,'Copy click must not wait for an API request');
  await page.evaluate(()=>{const box=document.createElement('textarea');box.id='paste-check';document.body.appendChild(box);});
  const paste=page.locator('#paste-check');await paste.press('Control+V');
  const text=await paste.inputValue();
  assert.match(text,/^Name: Clipboard Test\nSymbol: COPY\nMetadata Uri: ipfs:\/\//);
  assert.equal(text.split('\n').length,3);
  assert.doesNotMatch(text,/Website:|Telegram:|\nX:/);
  await page.getByRole('textbox',{name:'X',exact:true}).fill('https://x.com/example_updated');
  await page.waitForFunction(()=>Array.from(document.querySelectorAll('button')).some(b=>b.textContent==='Copy config'&&!b.disabled));
  await page.getByRole('button',{name:'Copy config',exact:true}).click();
  await page.getByRole('button',{name:'Copied',exact:true}).waitFor();
  await paste.fill('');await paste.press('Control+V');
  const updated=await paste.inputValue();
  assert.equal(updated.split('\n').length,3);
  assert.notEqual(updated,text,'Changing a social must produce a new metadata URI');
  await page.getByRole('textbox',{name:'X',exact:true}).fill('javascript:alert(1)');
  await page.waitForFunction(()=>Array.from(document.querySelectorAll('span')).some(n=>n.textContent==='social link must be HTTP(S)'));
  assert.equal(await page.getByRole('button',{name:'Copy config',exact:true}).isDisabled(),true);
  assert.deepEqual(errors,[]);
  const report={status:'passed',http_without_secure_context:true,clipboard_permission_grants:false,copy_inside_click:true,native_paste_matches:true,compact_note_lines:3,social_edit_changes_uri:true,original_metadata_uri:text.split('Metadata Uri: ')[1],updated_metadata_uri:updated.split('Metadata Uri: ')[1],slow_validation_before_copy:true,invalid_config_blocked:true,displayed_fee_dollars:1,page_errors:errors};
  fs.writeFileSync(process.env.WEB_TEST_OUT+'/clipboard-http.json',JSON.stringify(report,null,2)+'\n');
  console.log(JSON.stringify(report));
 } finally {await browser.close();}
})().catch(e=>{console.error(e);process.exit(1);});
