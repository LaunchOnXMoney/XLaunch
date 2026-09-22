class Component extends DCLogic {
  state = {sort:'Market Cap', query:'', tab:'Graduated', copied:null, page:1, result:{tokens:[],page:1,pages:1,total:0}, counts:{raising:0,graduated:0,total:0}, config:null, now:Date.now(), buy:null, wallet:'', note:'', noteError:'', checking:false, noteCopied:false};
  componentDidMount() {
    this._alive=true; this.reload();
    fetch(window.xlaunchApi('/api/config')).then(r=>{if(!r.ok)throw new Error('Configuration unavailable');return r.json();}).then(config=>{if(this._alive)this.setState({config});}).catch(console.error);
    this._poll=setInterval(()=>{if(!document.hidden)this.reload();},5000);
    this._clock=setInterval(()=>this.setState({now:Date.now()}),1000);
  }
  componentWillUnmount() {
    this._alive=false;clearInterval(this._poll);clearInterval(this._clock);
    clearTimeout(this._query);clearTimeout(this._ct);clearTimeout(this._noteTimer);clearTimeout(this._noteFlash);
    this._abort?.abort();this._noteAbort?.abort();
    document.removeEventListener('keydown',this._escape);
  }
  openBuy(token) {
    // Escape closes the modal, matching how a dialog is normally dismissed.
    this._escape=this._escape||(event=>{if(event.key==='Escape')this.closeBuy();});
    document.addEventListener('keydown',this._escape);
    this.setState({buy:token,wallet:'',note:'',noteError:'',checking:false,noteCopied:false});
  }
  closeBuy() {
    document.removeEventListener('keydown',this._escape);
    clearTimeout(this._noteTimer);this._noteAbort?.abort();
    this.setState({buy:null,wallet:'',note:'',noteError:'',checking:false,noteCopied:false});
  }
  changeWallet(value) {
    clearTimeout(this._noteTimer);this._noteAbort?.abort();
    const wallet=value.trim();
    this.setState({wallet,note:'',noteError:'',noteCopied:false,checking:wallet.length>0},()=>{
      if(this.state.wallet)this._noteTimer=setTimeout(()=>this.requestNote(),400);
    });
  }
  // The server builds the note and proves it parses back to this token and
  // wallet, so the page never assembles a payment note itself.
  async requestNote() {
    const {buy,wallet}=this.state;
    if(!buy||!wallet)return;
    this._noteAbort=new AbortController();const signal=this._noteAbort.signal;
    try {
      const response=await fetch(window.xlaunchApi('/api/buy-note'),{
        method:'POST',headers:{'Content-Type':'application/json'},
        body:JSON.stringify({mint:buy.mint,wallet}),signal});
      const result=await response.json();
      if(!response.ok)throw new Error(result.error||'That wallet was not accepted');
      if(this._alive&&!signal.aborted&&this.state.wallet===wallet)
        this.setState({note:result.note,noteError:'',checking:false});
    } catch(error) {
      if(error.name!=='AbortError'&&this._alive&&!signal.aborted)
        this.setState({note:'',noteError:error.message,checking:false});
    }
  }
  async copyBuyNote() {
    const {note}=this.state;
    if(!note)return;
    try {
      await window.xlaunchCopy(note);
      this.setState({noteCopied:true});
      clearTimeout(this._noteFlash);
      this._noteFlash=setTimeout(()=>this.setState({noteCopied:false}),1500);
    } catch(error) {this.setState({noteError:error.message});}
  }
  async reload() {
    const request=(this._request||0)+1;this._request=request;
    this._abort?.abort();this._abort=new AbortController();
    const sort={'Market Cap':'marketcap','Recent':'recent','Backers':'backers'}[this.state.sort];
    const params=new URLSearchParams({status:this.state.tab.toLowerCase(),sort,q:this.state.query,page:String(this.state.page),per_page:'6'});
    try {
      const [list,stats]=await Promise.all([fetch(window.xlaunchApi('/api/tokens?'+params),{signal:this._abort.signal}),fetch(window.xlaunchApi('/api/stats'),{signal:this._abort.signal})]);
      if(!list.ok||!stats.ok)throw new Error('Launchpad data is unavailable');
      const [result,counts]=await Promise.all([list.json(),stats.json()]);
      if(this._alive && this._request===request)this.setState({result,counts,page:result.page});
    } catch(error) {if(error.name!=='AbortError')console.error(error);}
  }
  pick(update) {this.setState(update,()=>this.reload());}
  money(value) {if(value===null||value===undefined)return '—';return '$'+(value>=1e6?(value/1e6).toFixed(2)+'M':value>=1e3?(value/1e3).toFixed(value%1000?1:0)+'K':value.toLocaleString(undefined,{maximumFractionDigits:2}));}
  renderVals() {
    const {sort,query,tab,result,counts,copied,now,config,buy,wallet,note,noteError,checking,noteCopied}=this.state;
    const page=result.page, total=result.pages;
    const tokens=result.tokens.map(token=>{
      const raising=token.status!=='graduated';const isCopied=copied===token.mint;
      const ratio=Math.min(1,Math.max(0,token.raised_cents/token.cap_cents));
      const remaining=Math.max(0,Math.ceil((token.deadline*1000-now)/1000));
      const eta=token.status==='awaiting_deployment'?'Pending':remaining>=60?Math.ceil(remaining/60)+'m':remaining+'s';
      const socialLinks=[['website','Website'],['twitter','X'],['telegram','Telegram']].filter(([key])=>token.socials[key]).map(([key,label])=>({key,label,url:token.socials[key],website:key==='website',twitter:key==='twitter',telegram:key==='telegram',stop:e=>e.stopPropagation()}));
      const row={...token,image_url:window.xlaunchApi(token.image_url)};
      return {...row,initial:token.ticker[0],open:()=>this.openBuy(row),raising,graduated:!raising,marketCap:this.money(token.market_cap_usd),marketLabel:!raising&&token.price_stale?'Market Cap · Stale':'Market Cap',marketHint:token.market_cap_usd==null?'Market cap unavailable':raising?'Estimated from the local presale curve; total supply valuation':(token.price_stale?'Last known price. ':'')+'Jupiter price × minted supply · checked '+new Date(token.price_updated_at*1000).toLocaleTimeString(),pct:Math.round(ratio*100),dash:(326.7*(1-ratio)).toFixed(1),backers:token.backers.toLocaleString(),eta,socialLinks,hasSocials:socialLinks.length>0,isCopied,notCopied:!isCopied,copyFg:isCopied?'#8ECDFF':'#8B98A5',copy:async(e)=>{e.stopPropagation();try{await window.xlaunchCopy(token.mint);this.setState({copied:token.mint});clearTimeout(this._ct);this._ct=setTimeout(()=>this.setState({copied:null}),1500);}catch(error){console.error(error);}}};
    });
    const tabs=['Raising','Graduated'].map(label=>({label,count:counts[label.toLowerCase()],fg:label===tab?'#E7E9EA':'#8B98A5',bar:label===tab?'#8ECDFF':'transparent',pick:()=>this.pick({tab:label,sort:'Market Cap',page:1})}));
    const sorts=['Market Cap','Recent','Backers'].map(label=>({label,bg:label===sort?'#EFF3F4':'transparent',fg:label===sort?'#0F1419':'#8B98A5',pick:e=>{e.stopPropagation();this.pick({sort:label,page:1});}}));
    const pages=Array.from({length:total},(_,i)=>({n:i+1,bg:i+1===page?'#EFF3F4':'transparent',fg:i+1===page?'#0F1419':'#8B98A5',go:()=>this.pick({page:i+1})}));
    const buySteps=[
      {isWallet:true,title:'Paste your Solana wallet',body:'Your tokens are sent to this address after the raise closes. Nothing is connected and nothing is signed.'},
      {isCopy:true,title:'Copy the note',body:'It names the token and your wallet. That pairing is what credits the purchase to you.'},
      {isSend:true,title:'Send any amount on X Money',body:'Pay '+(config?.receiver_handle||'@LaunchOnXMoney')+' with the note attached. Your contribution appears here once it settles.'}
    ];
    const buyReady=!!note;
    return {...window.xlaunchSiteLinks(config),
      buyOpen:!!buy,
      buy:buy||{mint:'',name:'',ticker:'',image_url:''},
      buySteps,wallet,
      walletErrorClass:noteError?'is-error':'',
      noteWalletShown:note?wallet:(wallet||'Your Solana address'),
      noteFg:buyReady?'#E7E9EA':'#6B7A88',
      buyHint:noteError||(checking?'Checking the address…':buyReady?'Note ready. Paste it as the payment note.':'Paste the wallet that should receive the tokens.'),
      buyHintFg:noteError?'#F4212E':'#8B98A5',
      buyNotReady:!buyReady,
      buyBtnBg:buyReady?'#8ECDFF':'rgba(142,205,255,.08)',
      buyBtnFg:buyReady?'#0F1419':'#8B98A5',
      buyBtnLabel:noteCopied?'Copied':'Copy note',
      xMoneyHref:config?.x_money_url||window.xlaunchApi('/go/x-money'),
      onWallet:e=>this.changeWallet(e.target.value),
      copyBuyNote:()=>this.copyBuyNote(),
      closeBuy:()=>this.closeBuy(),
      stopInside:e=>e.stopPropagation(),
      tokens,tabs,sorts,query,pages,prevDisabled:page<=1,nextDisabled:page>=total,prevFg:page<=1?'rgba(142,205,255,.25)':'#E7E9EA',nextFg:page>=total?'rgba(142,205,255,.25)':'#E7E9EA',prevPage:()=>this.pick({page:Math.max(1,page-1)}),nextPage:()=>this.pick({page:Math.min(total,page+1)}),onQuery:e=>{this.setState({query:e.target.value,page:1});clearTimeout(this._query);this._query=setTimeout(()=>this.reload(),180);}};
  }
}
