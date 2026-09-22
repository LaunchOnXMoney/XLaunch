const {chromium}=require('playwright');
const fs=require('fs');
const assert=require('assert/strict');
(async()=>{
 for(const name of ['WEB_TEST_URL','WEB_TEST_MINT','WEB_TEST_IMAGE','WEB_TEST_OUT'])assert.ok(process.env[name],name+' must be set');
 fs.mkdirSync(process.env.WEB_TEST_OUT,{recursive:true,mode:0o700});
 const browser=await chromium.launch({headless:true,executablePath:process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH,args:['--no-sandbox']});
 try {
  const context=await browser.newContext({viewport:{width:1440,height:1000},permissions:['clipboard-read','clipboard-write']});
  const page=await context.newPage();const errors=[];const apiCalls=[];page.on('request',r=>{if(r.url().includes('/api/'))apiCalls.push(r.url());});page.on('pageerror',e=>errors.push(e.message));
  await page.goto(process.env.WEB_TEST_URL);
  await page.getByRole('link',{name:'Deploy now',exact:true}).waitFor();
  assert.equal(await page.getByRole('button',{name:'Raising',exact:true}).count(),0);
  assert.equal(apiCalls.some(url=>url.includes('/api/tokens')),false,'home should not request listings');
  await page.getByRole('link',{name:'Explore',exact:true}).first().click();
  assert.equal(new URL(page.url()).pathname,'/explore');
  await page.getByRole('button',{name:'Raising',exact:true}).click();
  await page.getByText('XLaunch Integration Test',{exact:true}).waitFor();
  await page.waitForFunction(()=>{
   const img=document.querySelector('img[alt="XLaunch Integration Test"]');
   return img&&img.complete&&img.naturalWidth===16;
  });
  const avatar=page.getByRole('img',{name:'XLaunch Integration Test',exact:true});
  assert.match(await avatar.getAttribute('src'),/^\/media\/.*\.png$/);
  for(const [label,url] of [['Website','https://example.com'],['X','https://x.com/example'],['Telegram','https://t.me/example']]){
   const anchor=page.getByRole('link',{name:label,exact:true});assert.equal(await anchor.getAttribute('href'),url);assert.equal(await anchor.getAttribute('target'),'_blank');
  }
  await page.getByTitle('Copy contract address').click();
  await page.waitForFunction(mint=>navigator.clipboard.readText().then(s=>s===mint),process.env.WEB_TEST_MINT);
  await page.screenshot({path:process.env.WEB_TEST_OUT+'/launchpad.png',fullPage:true});
  const marketLabel=page.locator('[data-market-cap] + span');
  await marketLabel.waitFor();
  assert.equal(await marketLabel.innerText(),'Market Cap');
  const cap=await page.locator('[data-market-cap]').innerText();
  assert.notEqual(cap,'$125.5');assert.notEqual(cap,'$125.50');assert.notEqual(cap,'—');
  await page.getByRole('link',{name:'Launch Coin',exact:true}).click();
  await page.getByPlaceholder('Grok Coin',{exact:true}).fill('Social Test');
  await page.getByPlaceholder('GROK',{exact:true}).fill('SOCIAL');
  await page.getByRole('textbox',{name:'Website',exact:true}).fill('https://example.com');
  await page.getByRole('textbox',{name:'X',exact:true}).fill('https://x.com/example');
  await page.getByRole('textbox',{name:'Telegram',exact:true}).fill('https://t.me/example');
  await page.locator('input[type=file]').setInputFiles(process.env.WEB_TEST_IMAGE);
  const copy=page.getByRole('button',{name:'Copy Config',exact:true});
  await page.waitForFunction(()=>Array.from(document.querySelectorAll('button')).some(b=>b.textContent==='Copy Config'&&!b.disabled));
  await copy.click();
  await page.getByRole('button',{name:'Copied',exact:true}).waitFor();
  const config=await page.evaluate(()=>navigator.clipboard.readText());
  assert.match(config,/^Name: Social Test\nSymbol: SOCIAL\nMetadata Uri: ipfs:\/\//);
  assert.equal(config.split('\n').length,3);
  assert.doesNotMatch(config,/Website:|Telegram:|\nX:/);
  await page.screenshot({path:process.env.WEB_TEST_OUT+'/deploy.png',fullPage:true});
  assert.deepEqual(errors,[]);
  fs.writeFileSync(process.env.WEB_TEST_OUT+'/browser.json',JSON.stringify({status:'passed',separate_explore_page:true,home_does_not_fetch_tokens:true,curve_market_cap_shown:true,avatar_loaded_from_local_cache:true,social_icons:3,full_mint_copied:true,pinata_upload_in_browser:true,compact_note_lines:3,copied_note:config,page_errors:errors},null,2)+'\n');
  console.log('Browser: list, cached avatar, social links, real mint copy, upload, config copy passed.');
 } finally {await browser.close();}
})().catch(e=>{console.error(e);process.exit(1);});
