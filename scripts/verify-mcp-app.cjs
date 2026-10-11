// Original synthetic desktop integration runner, MIT OR Apache-2.0.
// No client SDK, package install, persistent MCP configuration or private media required.
const {spawn} = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const net = require('node:net');
const assert = require('node:assert/strict');
const [cli, root, port = '9876'] = process.argv.slice(2);
if (!cli || !root || !path.isAbsolute(root)) throw Error('usage: node verify-mcp-app.cjs <cli> <absolute workspace> [port]');
const run = 'qa-' + Date.now();
const evidence = {run, input: 'synthetic.mp4', protocol: '2025-11-25', checks: [], calls: []};
const child = spawn(path.resolve(cli), ['mcp', '--bridge', `127.0.0.1:${port}`, '--workspace', root], {stdio: ['pipe', 'pipe', 'pipe'], env: {...process.env, RAYON_NUM_THREADS: '1'}});
let next = 1, buffer = '', stderr = '';
const pending = new Map();
child.stderr.on('data', b => { stderr += b; });
child.stdout.on('data', b => {
  buffer += b;
  while (buffer.includes('\n')) {
    const at = buffer.indexOf('\n'), line = buffer.slice(0, at); buffer = buffer.slice(at + 1);
    if (!line.trim()) continue;
    let reply;
    try { reply = JSON.parse(line); } catch (e) { for (const p of pending.values()) p.reject(e); continue; }
    const p = pending.get(reply.id);
    if (p) { clearTimeout(p.timer); pending.delete(reply.id); p.resolve(reply); }
  }
});
child.on('error', e => { for (const p of pending.values()) p.reject(e); });
child.on('exit', code => { for (const p of pending.values()) p.reject(Error(`MCP exited ${code}: ${stderr}`)); });
function rpc(method, params) {
  const id = next++;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { pending.delete(id); reject(Error(`MCP timeout ${method}`)); }, 120000);
    pending.set(id, {resolve, reject, timer});
    child.stdin.write(JSON.stringify({jsonrpc:'2.0', id, method, params}) + '\n');
  });
}
async function call(name, args = {}) {
  const reply = await rpc('tools/call', {name, arguments:args});
  evidence.calls.push({name, arguments:args, response:reply});
  if (reply.error) return {protocol_error:reply.error};
  const content = reply.result?.content?.find(c => c.type === 'text')?.text;
  let result;
  try { result = JSON.parse(content); } catch { result = {ok:!reply.result?.isError, text:content}; }
  return result;
}
function check(name, condition, detail) {
  assert.ok(condition, `${name}: ${JSON.stringify(detail)}`);
  evidence.checks.push(name);
  console.log(`ok ${evidence.checks.length}: ${name}`);
}
async function control(method, params = {}) {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection({host:'127.0.0.1', port:Number(port)}), id = next++;
    let data = '';
    socket.setTimeout(30000, () => socket.destroy(Error('control timeout')));
    socket.on('error', reject);
    socket.on('connect', () => socket.write(JSON.stringify({id, method, params}) + '\n'));
    socket.on('data', b => {
      data += b;
      if (data.includes('\n')) {
        socket.end();
        try {
          const reply = JSON.parse(data.split('\n')[0]);
          if (!reply.ok) reject(Error(reply.error)); else resolve(reply.result);
        } catch (e) { reject(e); }
      }
    });
  });
}
async function read() {
  const r = await call('project_read_v1');
  assert.equal(r.ok, true, JSON.stringify(r));
  return r.project;
}
async function mutate(name, args, confirm = false) {
  const project = await read();
  const request = {...args, guard:{request_id:`${run}-${next}`, revision:project.revision, confirm}};
  const result = await call(name, request);
  return {request, result};
}
async function timeline() {
  const r = await call('timeline_read_v1');
  assert.equal(r.ok, true, JSON.stringify(r));
  return r.sequence;
}
async function waitJob(job) {
  for (let i = 0; i < 240; i++) {
    const r = await call('export_status_v1', {job});
    assert.equal(r.ok, true, JSON.stringify(r));
    if (r.job.finished) return r.job;
    await new Promise(resolve => setTimeout(resolve, 250));
  }
  throw Error('export did not finish in 60 seconds');
}
async function main() {
  const init = await rpc('initialize', {protocolVersion:evidence.protocol, capabilities:{}, clientInfo:{name:'filmcraft-synthetic-qa', version:'1'}});
  check('MCP initialize', init.result?.serverInfo?.name === 'filmcraft', init);
  child.stdin.write(JSON.stringify({jsonrpc:'2.0', method:'notifications/initialized'}) + '\n');
  const listed = await rpc('tools/list', {});
  const tools = listed.result?.tools || [];
  evidence.tools = tools;
  check('ten versioned tool schemas', tools.filter(t => t.name.endsWith('_v1')).length === 10, tools.map(t=>t.name));
  check('nested guard schema rejects unknown fields', tools.find(t=>t.name==='timeline_edit_v1')?.inputSchema?.$defs?.Guard?.additionalProperties === false, tools.find(t=>t.name==='timeline_edit_v1'));
  const cap = await call('app_capabilities_v1');
  check('real desktop bridge mode', cap.ok && cap.mode === 'live_app' && path.normalize(cap.workspace) === path.normalize(root), cap);
  let r = await mutate('project_write_v1', {action:'create', name:'MCP 三語 synthetic'}, true);
  check('create actual project', r.result.ok && r.result.project.name === 'MCP 三語 synthetic', r.result);
  const again = await call('project_write_v1', r.request);
  check('same request is not applied twice', again.replayed && again.project.revision === r.result.project.revision, again);
  const conflict = await call('project_write_v1', {...r.request, name:'should not replace'});
  check('different retry arguments rejected', conflict.error?.code === 'REQUEST_ID_CONFLICT', conflict);
  const stale = await call('project_write_v1', {...r.request, guard:{request_id:run+'-stale', revision:r.request.guard.revision}, name:'stale'});
  check('stale revision rejected', stale.error?.code === 'REVISION_CONFLICT', stale);
  r = await mutate('timeline_edit_v1', {edit:{action:'create_sequence', name:'Synthetic 24fps', width:320, height:180, fps:24}});
  check('sequence exists in shared App', r.result.ok && (await timeline()).video.length === 2, r.result);
  const sequenceId = (await timeline()).id;
  r = await mutate('history_v1',{action:'undo'});
  check('undo sequence creation removes it from the App',r.result.ok && (await read()).activeSequence === null,r.result);
  r = await mutate('history_v1',{action:'redo'});
  check('redo sequence creation restores its stable id',r.result.ok && (await read()).root.children.some(c=>c.item===sequenceId),r.result);
  r = await mutate('timeline_edit_v1',{edit:{action:'activate_sequence',sequence:sequenceId}});
  check('restored sequence can be reactivated for editing',r.result.ok && (await timeline()).id===sequenceId,r.result);
  r = await mutate('timeline_edit_v1',{edit:{action:'activate_sequence',sequence:9007199254740991}});
  check('unknown sequence is rejected without changing active timeline',!r.result.ok && (await timeline()).id===sequenceId,r.result);
  r = await mutate('media_import_v1', {paths:['synthetic.mp4']});
  check('import synthetic audiovisual media', r.result.ok, r.result);
  const imported = r;
  const stateAfterImport = await read();
  const importedAgain = await call('media_import_v1', imported.request);
  check('import retry creates no duplicate item', importedAgain.replayed && (await read()).revision === stateAfterImport.revision, importedAgain);
  const tree = JSON.stringify(stateAfterImport);
  const id = r.result.result.items?.[0]?.id ?? r.result.result.items?.[0] ?? r.result.result.imported?.[0]?.id ?? r.result.result.ids?.[0];
  evidence.importResult = r.result;
  assert.ok(Number.isSafeInteger(id), `no imported item id: ${JSON.stringify(r.result.result)}; ${tree}`);
  r = await mutate('timeline_edit_v1', {edit:{action:'place', item:id, track:'V1', audio_track:'A1', frame:0, insert:true}});
  check('linked video/audio placed in real timeline', r.result.ok && r.result.result.clips.length === 2, r.result);
  const clip = r.result.result.clips[0];
  r = await mutate('timeline_edit_v1', {edit:{action:'split', clip, frame:24}});
  check('split creates linked cuts', r.result.ok && r.result.result.cuts === 2, r.result);
  let q = await timeline();
  check('actual timeline reports two video clips', q.video[0].items.length === 2, q);
  r = await mutate('history_v1', {action:'undo'});
  check('App undo restores unsplit timeline', r.result.ok && (await timeline()).video[0].items.length === 1, r.result);
  r = await mutate('history_v1', {action:'redo'});
  check('App redo restores split timeline', r.result.ok && (await timeline()).video[0].items.length === 2, r.result);
  q = await timeline();
  const secondClip = q.video[0].items[1].clip;
  r = await mutate('timeline_edit_v1', {edit:{action:'trim',clip:secondClip,edge:'out',delta_frames:-2,ripple:false}});
  check('trim changes actual linked clip duration', r.result.ok && (await timeline()).video[0].items[1].durationFrames === 22, r.result);
  r = await mutate('timeline_edit_v1', {edit:{action:'move',clip:secondClip,track:'V2',time_ticks:254016000000,insert:true}});
  check('move preserves stable clip id on another track', r.result.ok && (await timeline()).video[1].items.some(c=>c.clip===secondClip), r.result);
  await mutate('history_v1', {action:'undo'});
  await mutate('history_v1', {action:'undo'});
  check('undo move and trim restores the short timeline', (await timeline()).video[0].items[1].durationFrames === 24, await timeline());
  r = await mutate('timeline_edit_v1', {edit:{action:'delete',clips:[secondClip]}}, true);
  check('confirmed delete affects App video and linked audio', r.result.ok && (await timeline()).video[0].items.length === 1 && (await timeline()).audio[0].items.length === 1, r.result);
  r = await mutate('history_v1', {action:'undo'});
  check('undo delete restores stable ids', r.result.ok && (await timeline()).video[0].items.some(c=>c.clip===secondClip), r.result);
  const beforeBad = await read();
  r = await mutate('media_import_v1', {paths:['../private.mp4']});
  check('workspace escape rejected without project change', r.result.error?.code === 'WORKSPACE_VIOLATION' && (await read()).revision === beforeBad.revision, r.result);
  if (process.platform === 'win32') {
    // A Windows junction must not turn an in-scope filename into an external write.
    // Only freshly created, empty temporary directories are used and removed.
    const outside = fs.mkdtempSync(path.join(require('node:os').tmpdir(),'filmcraft-junction-'));
    const junction = path.join(root,`${run}-junction`);
    try {
      fs.symlinkSync(outside,junction,'junction');
      r = await mutate('project_write_v1',{action:'save',path:`${run}-junction/escape.fcproj`});
      check('Windows junction escape rejected',r.result.error?.code === 'WORKSPACE_VIOLATION',r.result);
      check('junction rejection leaves App and external directory unchanged',
        (await read()).revision === beforeBad.revision && fs.readdirSync(outside).length === 0);
    } finally {
      if (fs.existsSync(junction)) fs.rmdirSync(junction);
      fs.rmdirSync(outside);
    }
  }
  const raw = await call('command_run', {id:'file.newProject', params:{name:'bypass'}});
  check('legacy raw command cannot bypass workspace guard', raw.ok === false && raw.text?.includes('unsupported'), raw);
  r = await mutate('timeline_edit_v1', {edit:{action:'split',clip:9007199254740991,frame:24}});
  check('unknown stable clip id rejected', r.result.error?.code === 'UNKNOWN_CLIP', r.result);
  const bad = await call('timeline_edit_v1', {guard:{request_id:run+'-unknown',revision:beforeBad.revision}, edit:{action:'split',clip,frame:24,typo:1}});
  check('unknown nested parameter rejected', !!bad.protocol_error || (bad.ok === false && bad.text?.includes('unknown field')), bad);
  r = await mutate('timeline_edit_v1', {edit:{action:'delete', clips:[clip]}});
  check('deletion needs explicit confirmation', r.result.error?.code === 'CONFIRMATION_REQUIRED', r.result);
  const projectPath = `${run}.fcproj`;
  r = await mutate('project_write_v1', {action:'save',path:projectPath});
  check('save actual App project file', r.result.ok && fs.existsSync(path.join(root,projectPath)), r.result);
  const direct = await control('engine.execute', {command:'project.inspect',params:{}});
  check('MCP and App control read identical project state', JSON.stringify(direct) === JSON.stringify(await read()), {direct});
  const ui = await call('ui_inspect'), directUi = await control('ui.inspect');
  check('MCP UI inspection reads actual desktop language',ui.ui?.language === directUi.ui.language,ui);
  const screenshot = await rpc('tools/call',{name:'ui_screenshot',arguments:{max_side:320}});
  const image = screenshot.result?.content?.find(c=>c.type==='image');
  const png = image && Buffer.from(image.data,'base64');
  check('MCP screenshot returns bounded actual App pixels',image?.mimeType==='image/png' && png?.subarray(1,4).toString()==='PNG' && png.readUInt32BE(16)<=320 && png.readUInt32BE(20)<=320,{mimeType:image?.mimeType,bytes:png?.length});
  fs.writeFileSync(path.join(root,`${run}-mcp-ui.png`),png);
  r = await mutate('project_write_v1', {action:'save',path:projectPath});
  check('existing file overwrite needs confirmation', r.result.error?.code === 'CONFIRMATION_REQUIRED', r.result);
  const relativeDocument = JSON.parse(fs.readFileSync(path.join(root,projectPath),'utf8'));
  let relativeReferences = 0;
  function relativize(value) {
    if (!value || typeof value !== 'object') return;
    for (const [key,item] of Object.entries(value)) {
      if (key === 'path' && typeof item === 'string' && path.basename(item) === 'synthetic.mp4') {
        value[key] = 'synthetic.mp4'; relativeReferences++;
      } else relativize(item);
    }
  }
  relativize(relativeDocument);
  assert.ok(relativeReferences > 0);
  fs.writeFileSync(path.join(root,`${run}-relative.fcproj`),JSON.stringify(relativeDocument));
  const beforeRelative = await read();
  r = await mutate('project_write_v1', {action:'open',path:`${run}-relative.fcproj`});
  check('relative project references rejected before App loading', r.result.error?.code === 'WORKSPACE_VIOLATION' && (await read()).revision === beforeRelative.revision, r.result);
  r = await mutate('project_write_v1', {action:'open',path:projectPath});
  check('reopen saved real project', r.result.ok && (await timeline()).video[0].items.length === 2, r.result);
  const directoryPath = `${run}-directory.mp4`;
  fs.mkdirSync(path.join(root,directoryPath));
  r = await mutate('export_start_v1', {path:directoryPath,width:320,height:180,fps:24}, true);
  check('export directory cannot redirect the requested file',r.result.error?.code === 'INVALID_PATH',r.result);
  fs.rmdirSync(path.join(root,directoryPath));
  const output = `${run}.mp4`;
  r = await mutate('export_start_v1', {path:output,width:320,height:180,fps:24});
  check('start asynchronous App export', r.result.ok && Number.isSafeInteger(r.result.result.job), r.result);
  const exportRequest = r, job = r.result.result.job;
  const exportAgain = await call('export_start_v1', exportRequest.request);
  check('export retry returns same job', exportAgain.replayed && exportAgain.result.job === job, exportAgain);
  const done = await waitJob(job);
  evidence.output = path.join(root,output); evidence.export = done;
  check('short H264/AAC export completes', !done.result?.error && fs.statSync(evidence.output).size > 1024, done);
  const unknownJob = await call('export_status_v1', {job:9007199254740991});
  check('foreign job inaccessible', unknownJob.error?.code === 'UNKNOWN_JOB', unknownJob);
  r = await mutate('timeline_edit_v1', {edit:{action:'place',item:id,track:'V1',audio_track:'A1',frame:48,duration_ticks:254016000000*30,insert:true}});
  check('prepare cancellable synthetic export', r.result.ok, r.result);
  r = await mutate('export_start_v1', {path:`${run}-cancel.mp4`,width:640,height:360,fps:24});
  check('cancellable App job starts', r.result.ok, r.result);
  const cancelJob = r.result.result.job;
  r = await mutate('export_cancel_v1', {job:cancelJob});
  check('cancellation requires confirmation', r.result.error?.code === 'CONFIRMATION_REQUIRED', r.result);
  r = await mutate('export_cancel_v1', {job:cancelJob}, true);
  check('confirmed synthetic job cancellation', r.result.ok, r.result);
  const cancelAgain = await call('export_cancel_v1',r.request);
  check('cancel retry is once only',cancelAgain.replayed && cancelAgain.ok,cancelAgain);
  const cancelled = await waitJob(cancelJob); evidence.cancelled = cancelled;
  check('cancelled job finishes with no partial output', !!cancelled.result?.error && !fs.existsSync(path.join(root,`${run}-cancel.mp4`)), cancelled);
  r = await mutate('history_v1', {action:'undo'}); // Remove only the 30-second generated test extension.
  check('restore short sequence after cancel test', r.result.ok && (await timeline()).durationFrames === 48, await timeline());
  const failPath = `${run}-fail.mp4`;
  fs.writeFileSync(path.join(root,failPath),'Synthetic protected output fixture only.');
  fs.chmodSync(path.join(root,failPath),0o444); // Windows readonly flag: actual encoder I/O failure.
  r = await mutate('export_start_v1', {path:failPath,width:320,height:180,fps:24}, true);
  const failure = r.result.ok ? await waitJob(r.result.result.job) : r.result;
  evidence.failure = failure;
  check('output failure reported and App survives', !!failure.error || !!failure.result?.error, failure);
  fs.chmodSync(path.join(root,failPath),0o600);
  fs.unlinkSync(path.join(root,failPath));
  r = await mutate('project_write_v1', {action:'save',path:projectPath}, true);
  check('confirmed save after failure succeeds', r.result.ok && !(await read()).dirty, r.result);
  check('saved project name survives rejected commands', (await read()).name === run, await read());
}
main().then(() => {evidence.ok=true;}).catch(e => {evidence.ok=false;evidence.error=e.stack;process.exitCode=1;console.error(e);}).finally(() => {
  fs.writeFileSync(path.join(root,`${run}-evidence.json`), JSON.stringify(evidence,null,2));
  child.stdin.end();
  const timer=setTimeout(()=>child.kill(),2000); timer.unref();
  console.log(`Evidence: ${path.join(root,`${run}-evidence.json`)}`);
});
