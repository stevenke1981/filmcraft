// Real App screenshots over its trusted loopback control interface. No focus requests.
const net = require('node:net'), fs = require('node:fs'), path = require('node:path');
const assert = require('node:assert/strict');
const [root, port = '9876'] = process.argv.slice(2);
if (!root || !path.isAbsolute(root)) throw Error('usage: node capture-i18n-app.cjs <absolute workspace> [port]');
let id=1;
async function control(method,params={}) {
  return new Promise((resolve,reject)=>{
    const socket=net.createConnection({host:'127.0.0.1',port:Number(port)}); let data='';
    socket.setTimeout(30000,()=>socket.destroy(Error('control timeout'))); socket.on('error',reject);
    socket.on('connect',()=>socket.write(JSON.stringify({id:id++,method,params})+'\n'));
    socket.on('data',b=>{data+=b;if(data.includes('\n')){socket.end();try{const r=JSON.parse(data.split('\n')[0]);if(r.ok)resolve(r.result);else reject(Error(r.error));}catch(e){reject(e);}}});
  });
}
async function main(){
  const shots=[];
  const sequence=await control('engine.execute',{command:'sequence.inspect',params:{}});
  const clip=sequence.video?.[0]?.items?.[0]?.clip;
  assert.ok(clip,'synthetic timeline must contain a clip');
  await control('engine.execute',{command:'timeline.select',params:{clips:[clip],add:false,toggle:false}});
  await control('ui.click',{id:'panel.tab.EffectControls'});
  for(const [language,command] of [['en','english'],['zh-TW','traditionalChinese'],['ja','japanese']]){
    await control('engine.execute',{command:'app.language.'+command,params:{}});
    for(const [size,width,height] of [['desktop',1600,980],['compact',1100,720]]){
      await control('ui.resize',{width,height});
      await control('ui.set',{mode:'Edit',timeline:{fit:true}});
      await control('engine.execute',{command:'playhead.set',params:{frame:12}});
      const filename=`i18n-${language}-${size}.png`;
      await control('ui.screenshot',{path:path.join(root,filename)});
      const state=await control('ui.inspect'),elements=await control('ui.elements'),menus=await control('ui.menu.list');
      assert.equal(state.ui.language,language);
      assert.ok(menus.some(m=>m.id==='app.language.'+command && m.checked===true));
      const element=id=>elements.find(e=>e.id===id);
      const overlap=(a,b)=>a && b && a.rect[0]<b.rect[0]+b.rect[2] && b.rect[0]<a.rect[0]+a.rect[2] && a.rect[1]<b.rect[1]+b.rect[3] && b.rect[1]<a.rect[1]+a.rect[3];
      assert.ok(!overlap(element('header.menus'),element('header.discord')));
      assert.ok(!overlap(element('program.zoom'),element('program.resolution')));
      assert.ok(!overlap(element('program.timecode'),element('program.zoom')));
      if(size==='compact' && language==='ja') assert.ok(element('header.menus'),'Japanese compact menus remain reachable');
      shots.push({language,size,width,height,filename,state,elements,menus});
      console.log('captured '+filename);
    }
    await control('ui.resize',{width:1600,height:980});
    await control('ui.set',{mode:'Export',export:{location:root,fileName:'synthetic-preview'}});
    const filename=`i18n-${language}-export.png`;
    await control('ui.screenshot',{path:path.join(root,filename)});
    shots.push({language,size:'export',filename,state:await control('ui.inspect'),elements:await control('ui.elements')});
    console.log('captured '+filename);
  }
  await control('ui.set',{mode:'Edit'});
  fs.writeFileSync(path.join(root,'i18n-ui-evidence.json'),JSON.stringify(shots,null,2));
}
main().catch(e=>{console.error(e);process.exitCode=1;});
