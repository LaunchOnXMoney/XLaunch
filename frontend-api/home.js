class Component extends DCLogic {
  state={config:null,copied:false,hover:null};
  // Floating profile bubbles. Positions are percentages; depth 0..1 runs far to
  // near and sets size and opacity. A slot without a picture shows its initial.
  profiles=[
    {name:'Profile 1',handle:'handle1',avatar:'/profiles/profile-1.png',left:12,top:22,depth:.9},
    {name:'Profile 2',handle:'handle2',avatar:'/profiles/profile-2.png',left:22,top:62,depth:.55},
    {name:'Profile 3',handle:'handle3',avatar:'/profiles/profile-3.png',left:8,top:68,depth:.35},
    {name:'Profile 4',handle:'handle4',avatar:'/profiles/profile-4.png',left:84,top:20,depth:.7},
    {name:'Profile 5',handle:'handle5',avatar:'/profiles/profile-5.png',left:88,top:52,depth:1},
    {name:'Profile 6',handle:'handle6',avatar:'/profiles/profile-6.png',left:76,top:70,depth:.45},
    {name:'Profile 7',handle:'handle7',avatar:'/profiles/profile-7.png',left:30,top:14,depth:.4}
  ];
  canvasRef=window.React.createRef();
  target=null;cur=null;
  componentDidMount(){
    this._alive=true;
    fetch(window.xlaunchApi('/api/config')).then(r=>{if(!r.ok)throw new Error('Configuration unavailable');return r.json();})
      .then(config=>{if(this._alive)this.setState({config});}).catch(console.error);
    const cv=this.canvasRef.current;if(!cv)return;
    const ctx=cv.getContext('2d');
    // Wire grid: lines brighten and gain glow near the cursor, the glass-wire look.
    const loop=()=>{
      const w=cv.clientWidth,hgt=cv.clientHeight,dpr=Math.min(2,window.devicePixelRatio||1);
      if(cv.width!==w*dpr||cv.height!==hgt*dpr){cv.width=w*dpr;cv.height=hgt*dpr;}
      const tgt=this.target||{x:w/2,y:hgt/2};
      if(!this.cur)this.cur={...tgt};
      this.cur.x+=(tgt.x-this.cur.x)*0.25;this.cur.y+=(tgt.y-this.cur.y)*0.25;
      const cx=this.cur.x,cy=this.cur.y,R=200,cell=64,seg=16;
      ctx.setTransform(dpr,0,0,dpr,0,0);ctx.clearRect(0,0,w,hgt);
      const strokeSeg=(x1,y1,x2,y2)=>{
        const mx=(x1+x2)/2,my=(y1+y2)/2;
        const d=Math.hypot(mx-cx,my-cy);
        const t=Math.max(0,1-d/R),e=t*t;
        const vign=Math.max(0,1-Math.hypot((mx-w/2)/(w*.55),(my-hgt/2)/(hgt*.6)));
        const base=0.05*vign;
        if(base+e<0.01)return;
        ctx.beginPath();ctx.moveTo(x1,y1);ctx.lineTo(x2,y2);
        ctx.shadowBlur=0;ctx.lineWidth=e>0.05?1+e*0.6:1;
        const a=Math.min(1,base+e*.7);
        ctx.strokeStyle=e>0.05?`rgba(${Math.round(142+113*e)},${Math.round(205+50*e)},255,${a.toFixed(3)})`:'rgba(142,205,255,'+a.toFixed(3)+')';
        ctx.stroke();
      };
      for(let x=0;x<=w+cell;x+=cell)for(let y=0;y<hgt;y+=seg)strokeSeg(x,y,x,y+seg);
      for(let y=0;y<=hgt+cell;y+=cell)for(let x=0;x<w;x+=seg)strokeSeg(x,y,x+seg,y);
      this._raf=requestAnimationFrame(loop);
    };
    this._raf=requestAnimationFrame(loop);
  }
  componentWillUnmount(){this._alive=false;cancelAnimationFrame(this._raf);clearTimeout(this._t);}
  money(cents){return '$'+(cents/100).toLocaleString(undefined,{maximumFractionDigits:2});}
  renderVals(){
    const {config,copied,hover}=this.state;
    const links=window.xlaunchSiteLinks(config);
    const receiverHandle=links.receiverHandle;
    const orbs=this.profiles.map((p,i)=>{
      const open=hover===i;const av=Math.round(30+p.depth*22);const right=p.left>50;
      return {...p,key:i,open,
        left:right?'auto':p.left+'%',right:right?(100-p.left)+'%':'auto',
        dir:right?'row-reverse':'row',textPad:right?'0 0 0 6px':'0 6px 0 0',
        top:p.top+'%',av:av+'px',size:(av+12)+'px',pad:'0 6px',
        opacity:open?1:(.35+p.depth*.45).toFixed(2),scale:open?'1.06':'1',
        dur:(5+(i%4)*1.3).toFixed(1)+'s',delay:'-'+(i*0.9).toFixed(1)+'s',
        avatarBg:p.avatar?`url("${p.avatar}")`:'none',initialShown:p.avatar?'':p.name[0],
        enter:()=>this.setState({hover:i}),leave:()=>this.setState({hover:null})};
    });
    return {...links,orbs,canvasRef:this.canvasRef,
      launchFee:config?this.money(config.launch_fee_cents):'…',
      copyLabel:copied?'Copied':'Click to Copy',copyFg:copied?'#8ECDFF':'#8B98A5',
      handleTitle:copied?'Copied':'Copy the X Money handle',
      onMove:e=>{const r=e.currentTarget.getBoundingClientRect();this.target={x:e.clientX-r.left,y:e.clientY-r.top};},
      onLeave:()=>{this.target=null;},
      copyHandle:async()=>{try{await window.xlaunchCopy(receiverHandle);this.setState({copied:true});clearTimeout(this._t);this._t=setTimeout(()=>this.setState({copied:false}),1500);}catch(error){console.error(error);}}};
  }
}
