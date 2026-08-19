// WebRTC full-mesh voice + silence-gated 16 kHz WAV capture.
// Loaded by both pages; all fetch paths are relative so they resolve under
// / on the DM page and /p/ on a player page.
(() => {
const RMS_GATE = 0.012, HANG_MS = 700, MIN_MS = 400, MAX_MS = 15000;

let me = null, es = null, onPeers = null, cfg = { iceServers: [] };
let stream = null, ctx = null, node = null, joined = false, ooc = false;
const peers = new Map();   // peer id -> RTCPeerConnection
const muted = new Set();   // peer ids we locally muted
const pendingIce = new Map(); // peer id -> candidates queued before the remote description landed

function post(type, to, payload){
  return fetch('rtc', {method:'POST', headers:{'Content-Type':'application/json'},
    body: JSON.stringify({to, type, payload})}).catch(()=>{});
}

function notify(){
  if(onPeers) onPeers([...peers.keys()].map(id =>
    ({id, muted: muted.has(id), state: peers.get(id).connectionState || ''})));
}

function audioEl(id){
  let a = document.getElementById('aud-'+id);
  if(!a){
    a = document.createElement('audio');
    a.id = 'aud-'+id; a.autoplay = true; a.hidden = true;
    document.body.append(a);
  }
  return a;
}

function drop(id){
  const p = peers.get(id);
  if(p){ p.close(); peers.delete(id); }
  pendingIce.delete(id);
  const a = document.getElementById('aud-'+id);
  if(a) a.remove();
  notify();
}

function pc(id){
  let p = peers.get(id);
  if(p) return p;
  p = new RTCPeerConnection(cfg);
  for(const t of stream.getTracks()) p.addTrack(t, stream);
  p.onicecandidate = e => { if(e.candidate) post('ice', id, e.candidate); };
  p.ontrack = e => {
    const a = audioEl(id);
    a.srcObject = e.streams[0];
    a.muted = muted.has(id);
  };
  p.onconnectionstatechange = () => {
    if(p.connectionState === 'failed' || p.connectionState === 'closed') drop(id);
    else notify();
  };
  peers.set(id, p);
  notify();
  return p;
}

async function offerTo(id){
  const p = pc(id);
  await p.setLocalDescription(await p.createOffer());
  post('offer', id, p.localDescription);
}

async function flushIce(id, p){
  for(const c of pendingIce.get(id) || []) await p.addIceCandidate(c).catch(()=>{});
  pendingIce.delete(id);
}

async function onRtc(e){
  if(!joined) return;
  const m = JSON.parse(e.data);
  const {from, type, payload} = m;
  if(!from || from === me) return;
  try{
    if(type === 'join'){ await offerTo(from); return; }  // existing peers call the newcomer
    if(type === 'leave'){ drop(from); return; }
    const p = pc(from);
    if(type === 'offer'){
      if(p.signalingState === 'have-local-offer'){
        // glare (double-join): larger id rolls back and answers
        if(me < from) return;
        await p.setLocalDescription({type:'rollback'});
      }
      await p.setRemoteDescription(payload);
      await flushIce(from, p);
      await p.setLocalDescription(await p.createAnswer());
      post('answer', from, p.localDescription);
    }else if(type === 'answer'){
      if(p.signalingState === 'have-local-offer'){
        await p.setRemoteDescription(payload);
        await flushIce(from, p);
      }
    }else if(type === 'ice'){
      // candidates can outrun the offer/answer over the relay — queue until
      // the remote description is in place, then flush
      if(p.remoteDescription){
        await p.addIceCandidate(payload).catch(()=>{});
      }else{
        if(!pendingIce.has(from)) pendingIce.set(from, []);
        pendingIce.get(from).push(payload);
      }
    }
  }catch(err){ console.warn('rtc', type, err); }
}

// ---- capture: worklet taps raw samples; main thread gates and ships WAVs ----

const workletUrl = URL.createObjectURL(new Blob([`
registerProcessor('mh-tap', class extends AudioWorkletProcessor {
  process(inputs){
    const ch = inputs[0] && inputs[0][0];
    if(ch) this.port.postMessage(ch.slice(0));
    return true;
  }
});`], {type:'application/javascript'}));

function shipWav(bufs, frames, rate){
  const all = new Float32Array(frames);
  let o = 0;
  for(const b of bufs){ all.set(b, o); o += b.length; }
  const step = rate/16000, n = Math.floor(frames/step);
  const bytes = n*2;
  const wav = new ArrayBuffer(44+bytes);
  const dv = new DataView(wav);
  const tag = (off,s)=>{ for(let i=0;i<s.length;i++) dv.setUint8(off+i, s.charCodeAt(i)); };
  tag(0,'RIFF'); dv.setUint32(4, 36+bytes, true); tag(8,'WAVE');
  tag(12,'fmt '); dv.setUint32(16,16,true); dv.setUint16(20,1,true); dv.setUint16(22,1,true);
  dv.setUint32(24,16000,true); dv.setUint32(28,32000,true); dv.setUint16(32,2,true); dv.setUint16(34,16,true);
  tag(36,'data'); dv.setUint32(40, bytes, true);
  for(let i=0;i<n;i++){
    const s = Math.max(-1, Math.min(1, all[Math.floor(i*step)]));
    dv.setInt16(44+i*2, s<0 ? s*0x8000 : s*0x7fff, true);
  }
  const headers = {'Content-Type':'audio/wav'};
  if(ooc) headers['X-OOC'] = '1';
  fetch('audio', {method:'POST', headers, body:wav}).catch(()=>{});
}

async function startCapture(){
  ctx = new AudioContext();
  await ctx.audioWorklet.addModule(workletUrl);
  const src = ctx.createMediaStreamSource(stream);
  node = new AudioWorkletNode(ctx, 'mh-tap');
  src.connect(node);
  const rate = ctx.sampleRate;
  let bufs = [], frames = 0, recording = false, started = 0, lastLoud = 0;
  node.port.onmessage = e => {
    const ch = e.data;
    let sum = 0;
    for(let i=0;i<ch.length;i++) sum += ch[i]*ch[i];
    const rms = Math.sqrt(sum/ch.length);
    const now = ctx.currentTime*1000;
    if(rms > RMS_GATE) lastLoud = now;
    if(!recording){
      if(rms <= RMS_GATE) return;
      recording = true; started = now; bufs = []; frames = 0;
    }
    bufs.push(ch); frames += ch.length;
    if(now - lastLoud > HANG_MS){
      // utterance over: ship it if enough actual speech accumulated
      if(lastLoud - started >= MIN_MS) shipWav(bufs, frames, rate);
      recording = false; bufs = []; frames = 0;
    }else if(now - started > MAX_MS){
      // monologue cap: flush and keep rolling
      shipWav(bufs, frames, rate);
      started = now; bufs = []; frames = 0;
    }
  };
}

window.Voice = {
  async join(id, eventSource, peersCb){
    if(joined || me) return;  // already in (or mid-join) — don't re-acquire the mic
    me = id; es = eventSource; onPeers = peersCb;
    try{
      cfg = await (await fetch('rtc-config')).json();
      stream = await navigator.mediaDevices.getUserMedia(
        {audio:{echoCancellation:true, noiseSuppression:true}});
      es.addEventListener('rtc', onRtc);
      await startCapture();
      joined = true;
      post('join', '*', null);
      notify();
    }catch(err){
      // undo everything acquired so far so a retry starts clean
      if(es) es.removeEventListener('rtc', onRtc);
      if(node) node.port.onmessage = null;
      if(ctx) ctx.close();
      if(stream) stream.getTracks().forEach(t=>t.stop());
      me = es = onPeers = stream = ctx = node = null;
      joined = false;
      throw err;
    }
  },
  leave(){
    if(!me) return;
    post('leave', '*', null);
    es.removeEventListener('rtc', onRtc);
    for(const id of [...peers.keys()]) drop(id);
    if(node) node.port.onmessage = null;
    if(ctx) ctx.close();
    if(stream) stream.getTracks().forEach(t=>t.stop());
    me = es = onPeers = stream = ctx = node = null;
    joined = false;
  },
  // disabled track produces silence, so the RMS gate closes too — no WAVs ship
  selfMute(on){ if(stream) stream.getAudioTracks().forEach(t=>{ t.enabled = !on; }); },
  // advisory: marks uploaded utterances as out-of-character (X-OOC header)
  setOoc(on){ ooc = !!on; },
  mute(id, on){
    if(on) muted.add(id); else muted.delete(id);
    const a = document.getElementById('aud-'+id);
    if(a) a.muted = on;
    notify();
  },
};
})();
