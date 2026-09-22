class Component extends DCLogic {
  state={config:null};
  componentDidMount(){
    this._alive=true;
    fetch(window.xlaunchApi('/api/config')).then(r=>{if(!r.ok)throw new Error('Configuration unavailable');return r.json();})
      .then(config=>{if(this._alive)this.setState({config});}).catch(console.error);
  }
  componentWillUnmount(){this._alive=false;}
  money(cents){return '$'+(cents/100).toLocaleString(undefined,{maximumFractionDigits:2});}
  duration(seconds){
    if(seconds%3600===0)return (seconds/3600)+(seconds===3600?' hour':' hours');
    if(seconds%60===0)return (seconds/60)+' minutes';
    return seconds+' seconds';
  }
  renderVals(){
    const {config}=this.state;
    return {
      ...window.xlaunchSiteLinks(config),
      launchFee:config?this.money(config.launch_fee_cents):'…',
      raiseTarget:config?this.money(config.cap_cents):'…',
      deadline:config?this.duration(config.duration_seconds):'…'
    };
  }
}
