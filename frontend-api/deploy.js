class Component extends DCLogic {
  state = {name:'',symbol:'',imgSrc:null,fileName:'',uri:'',drag:false,copied:null,website:'',twitter:'',telegram:'',uploading:false,error:'',config:null,validatedKey:null,preparedUri:'',preparing:false};
  componentDidMount() {this._alive=true;fetch(window.xlaunchApi('/api/config')).then(r=>{if(!r.ok)throw new Error('Configuration unavailable');return r.json();}).then(config=>{if(this._alive)this.setState({config});}).catch(console.error);}
  componentWillUnmount() {this._alive=false;clearTimeout(this._t);clearTimeout(this._validateTimer);this._upload?.abort();this._validation?.abort();}
  change(update) {
    clearTimeout(this._validateTimer);this._validation?.abort();
    this.setState({...update,validatedKey:null,preparedUri:'',preparing:false,copied:null,error:''},()=>{
      if(this.state.name&&this.state.symbol&&this.state.uri&&!this.state.uploading){
        this.setState({preparing:true});
        this._validateTimer=setTimeout(()=>this.prepareConfig(),600);
      }
    });
  }
  async prepareConfig() {
    const config=this.configuration(),key=JSON.stringify(config);
    this._validation=new AbortController();const signal=this._validation.signal;
    try {
      const response=await fetch(window.xlaunchApi('/api/launch-config/prepare'),{method:'POST',headers:{'Content-Type':'application/json'},body:key,signal});
      const result=await response.json();if(!response.ok)throw new Error(result.error||'Invalid configuration');
      if(this._alive&&!signal.aborted&&key===JSON.stringify(this.configuration()))this.setState({validatedKey:key,preparedUri:result.uri,preparing:false,error:''});
    }catch(error){if(error.name!=='AbortError'&&this._alive&&!signal.aborted)this.setState({validatedKey:null,preparedUri:'',preparing:false,error:error.message});}
  }
  async load(file) {
    if(!file)return;
    if(!['image/png','image/jpeg'].includes(file.type)||file.size>=2*1024*1024){this.setState({error:'Use a square PNG or JPG under 2 MB.'});return;}
    this._upload?.abort();this._upload=new AbortController();const signal=this._upload.signal;
    this.change({uploading:true,uri:'',imgSrc:null,fileName:file.name});
    try {
      const response=await fetch(window.xlaunchApi('/api/images'),{method:'POST',headers:{'Content-Type':file.type},body:file,signal});
      const result=await response.json();if(!response.ok)throw new Error(result.error||'Upload failed');
      if(this._alive&&!signal.aborted)this.change({imgSrc:window.xlaunchApi(result.preview_url),uri:result.uri,uploading:false});
    } catch(error) {if(error.name!=='AbortError'&&this._alive)this.setState({uploading:false,error:error.message});}
  }
  flash(k) {this.setState({copied:k});clearTimeout(this._t);this._t=setTimeout(()=>this.setState({copied:null}),1500);}
  async copy(text,k) {try{await window.xlaunchCopy(text);this.flash(k);}catch(error){this.setState({error:error.message});}}
  configuration() {return {name:this.state.name,symbol:this.state.symbol.toUpperCase(),image_uri:this.state.uri,socials:{website:this.state.website.trim(),twitter:this.state.twitter.trim(),telegram:this.state.telegram.trim()}};}
  copyConfig() {
    const config=this.configuration();
    if(this.state.validatedKey!==JSON.stringify(config))return;
    const note=`Name: ${config.name}\nSymbol: ${config.symbol}\nMetadata Uri: ${this.state.preparedUri}`;
    // Copy directly in the click handler, with no network await before clipboard access.
    this.copy(note,'cfg');
  }
  renderVals() {
    const {name,symbol,imgSrc,fileName,uri,drag,copied,uploading,error,website,twitter,telegram,config,preparedUri,preparing}=this.state;
    const sym=symbol.toUpperCase(),hasImage=!!imgSrc,ready=!!(name&&sym&&preparedUri&&!uploading&&this.state.validatedKey===JSON.stringify(this.configuration()));
    const socialFields=[['website','Website','https://your-site.com'],['twitter','X','https://x.com/yourcoin'],['telegram','Telegram','https://t.me/yourcoin']].map(([key,label,placeholder])=>({key,label,placeholder,value:this.state[key],change:e=>this.change({[key]:e.target.value})}));
    return {...window.xlaunchSiteLinks(config),xMoneyHref:window.xlaunchApi('/go/x-money'),launchFee:config?'$'+(config.launch_fee_cents/100).toLocaleString(undefined,{maximumFractionDigits:2}):'…',name,symbol:sym,imgSrc,fileName,hasImage,noImage:!hasImage,notReady:!ready,initial:(sym||name||'?')[0].toUpperCase(),previewName:name||'Your coin',previewSymbol:sym||'TICKER',nameShown:name||'{Name}',symbolShown:sym||'{Symbol}',uriShown:error||(uploading?'Uploading…':preparing?'Saving image and socials…':preparedUri||(hasImage?'Enter name and symbol':'{metadata_uri}')),nameFg:name?'#E7E9EA':'#6B7A88',symbolFg:sym?'#E7E9EA':'#6B7A88',uriFg:preparedUri?'#E7E9EA':'#6B7A88',dropBorder:drag?'#8ECDFF':'rgba(142,205,255,.25)',uriBtnLabel:copied==='uri'?'Copied':'Copy',uriBtnBg:ready?'#EFF3F4':'rgba(142,205,255,.08)',uriBtnFg:ready?'#0F1419':'#8B98A5',cfgBtnLabel:copied==='cfg'?'Copied':uploading?'Uploading…':preparing?'Saving…':'Copy Config',cfgBtnBg:ready?'#8ECDFF':'rgba(142,205,255,.08)',cfgBtnFg:ready?'#0F1419':'#8B98A5',onName:e=>this.change({name:e.target.value}),onSymbol:e=>this.change({symbol:e.target.value.replace(/[^a-z0-9]/gi,'')}),onFile:e=>this.load(e.target.files?.[0]),onDragOver:e=>{e.preventDefault();if(!drag)this.setState({drag:true});},onDragLeave:()=>this.setState({drag:false}),onDrop:e=>{e.preventDefault();this.setState({drag:false});this.load(e.dataTransfer.files?.[0]);},copyUri:()=>{if(ready)this.copy(preparedUri,'uri');},copyConfig:()=>this.copyConfig(),socialFields,website,twitter,telegram,hasWebsite:!!website,hasTwitter:!!twitter,hasTelegram:!!telegram,receiverHandle:window.xlaunchSiteLinks(config).receiverHandle,openMoney:()=>{if(config?.x_money_url)window.location.assign(config.x_money_url);else this.setState({error:'The X Money destination has not been configured yet.'});}};
  }
}
