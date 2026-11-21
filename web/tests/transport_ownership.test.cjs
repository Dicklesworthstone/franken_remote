
"use strict";
const test=require("node:test");
const assert=require("node:assert/strict");
const {available,deferred,nextTurn,ioFixture}=require("./browser_fixture.cjs");

async function start(client,f,index=0){
 const pending=client.connect("observer"); await nextTurn();
 f.sockets[index].open();await pending;
}

test("reentrant awaiting_transport replacement fences old bootstrap and timer", async()=>{
 const f=ioFixture();
 let client, replacement, changed=false;
 client=f.makeClient({onState(s){
  if(s.state_label==="awaiting_transport"&&!changed){changed=true;replacement=client.connect("observer");}
 }});
 const old=client.connect("observer");
 const cancelled=assert.rejects(old,{code:"browser_connection_cancelled"});
 await nextTurn(); await cancelled;
 assert.equal(f.sockets.length,1);assert.equal(f.cores[0].freed,1);
 f.sockets[0].open();await replacement;
 assert.equal(f.cores[1].freed,0);assert.equal(f.timers.size,1);
 client.disconnect();assert.equal(f.timers.size,0);
});

test("reentrant awaiting_host replacement retires an opening socket exactly once",async()=>{
 const f=ioFixture();
 let client,replacement,changed=false;
 client=f.makeClient({onState(s){
  if(s.state_label==="awaiting_host"&&!changed){changed=true;replacement=client.connect("observer");}
 }});
 const old=client.connect("observer");
 const cancelled=assert.rejects(old,{code:"browser_connection_cancelled"});
 await nextTurn();f.sockets[0].open();await cancelled;await nextTurn();
 assert.equal(f.sockets[0].closeCalls.length,1);assert.equal(f.cores[0].freed,1);
 f.sockets[1].open();await replacement;assert.equal(client.controlSocket,f.sockets[1]);
 assert.equal(f.timers.size,1);client.disconnect();
});

test("terminal Rust refusal still frees and closes when Rust disconnect rejects",async()=>{
 const f=ioFixture();const refusals=[];
 const client=f.makeClient({onRefused:e=>refusals.push(e)});
 await start(client,f);
 f.cores[0].receiveControl=()=>{f.cores[0].phase="refused";f.cores[0].token=null;return "host_required_capability_missing";};
 f.cores[0].disconnect=()=>{throw "browser_connection_cancelled";};
 f.sockets[0].message(new ArrayBuffer(0));
 assert.equal(client.closed,true);assert.equal(f.cores[0].freed,1);
 assert.equal(f.sockets[0].closeCalls.length,1);assert.equal(f.timers.size,0);
 assert.equal(refusals.length,1);assert.equal(refusals[0].code,"host_required_capability_missing");
});

test("reentrant refused-state replacement ignores original terminal callback",async()=>{
 const f=ioFixture();const refusals=[];
 let replacement,client;
 client=f.makeClient({onState(s){if(s.state_label==="refused")replacement=client.connect("observer");},onRefused:e=>refusals.push(e)});
 await start(client,f);
 f.cores[0].receiveControl=()=>{f.cores[0].phase="refused";f.cores[0].token=null;return "host_required_capability_missing";};
 f.cores[0].disconnect=()=>{throw "browser_connection_cancelled";};
 f.sockets[0].message(new ArrayBuffer(0));
 await nextTurn();f.sockets[1].open();await replacement;
 assert.equal(client.controlSocket,f.sockets[1]);assert.equal(f.cores[0].freed,1);
 assert.equal(f.cores[1].freed,0);assert.equal(refusals.length,0);
 assert.equal(f.timers.size,1);client.disconnect();
});

test("pagehide before module availability cancels and disposes late construction",async()=>{
 const loading=deferred();const f=ioFixture(available(),{createAdapter:()=>loading.promise});
 const closed=[];const client=f.makeClient({onClosed:c=>closed.push(c)});
 const pending=client.connect("observer");const cancelled=assert.rejects(pending,{code:"browser_connection_cancelled"});
 await nextTurn();client.lifecycle("pagehide");await cancelled;
 let freed=0;loading.resolve({core:{free(){freed++;}},maxBootstrapBytes:4096,maxControlRecordBytes:4096});
 await nextTurn();assert.equal(freed,1);assert.equal(f.requests.length,0);assert.equal(f.timers.size,0);
 assert.deepEqual(closed,["browser_connection_cancelled"]);
});

test("a never-settling reader cancellation cannot hang disconnect",async()=>{
 const body=deferred();const f=ioFixture();
 let cancelCalls=0,released=0;
 f.window.fetch=async()=>({ok:true,headers:{get:()=>null},body:{getReader:()=>({
   read:()=>body.promise,cancel:()=>{cancelCalls++;return new Promise(()=>{});},releaseLock:()=>released++
 })}});
 const client=f.makeClient();const pending=client.connect("observer");const cancelled=assert.rejects(pending,{code:"browser_connection_cancelled"});
 await nextTurn();client.disconnect();await cancelled;
 assert.ok(cancelCalls>=1);assert.equal(released,1);assert.equal(f.cores[0].freed,1);assert.equal(f.timers.size,0);
 body.resolve({done:true});await nextTurn();assert.equal(f.sockets.length,0);
});

test("bootstrap stream bounds are cumulative across chunks and reject before Rust parsing",async()=>{
 const f=ioFixture();let part=0,cancelled=0;
 f.window.fetch=async()=>({ok:true,headers:{get:()=>null},body:{getReader:()=>({
   read:async()=>part++<3?{done:false,value:new Uint8Array(2000)}:{done:true},
   cancel:()=>{cancelled++;return Promise.resolve();},releaseLock(){}
 })}});
 const client=f.makeClient();await assert.rejects(client.connect("observer"),{code:"browser_bootstrap_too_large"});
 assert.equal(part,3);assert.ok(cancelled>0);assert.equal(f.cores[0].calls.some(x=>x[0]==="acceptBootstrap"),false);
 assert.equal(f.cores[0].freed,1);assert.equal(f.timers.size,0);
});

test("peer exception text cannot reach the displayed connection error",async()=>{
 const f=ioFixture();f.window.fetch=async()=>{throw new Error("sensitive credential nonce=a5 cookie=secret");};
 const client=f.makeClient();
 await assert.rejects(client.connect("observer"),e=>{
  assert.equal(e.code,"browser_bootstrap_unreadable");
  assert.equal(e.message.includes("secret"),false);assert.equal(e.message.includes("nonce"),false);return true;
 });
 assert.equal(f.cores[0].freed,1);assert.equal(f.timers.size,0);
});

test("malformed UTF-8 cannot reach the Rust JSON parser",async()=>{
 const f=ioFixture(available(),{rawBody:Uint8Array.of(0xc0,0xaf)});const client=f.makeClient();
 await assert.rejects(client.connect("observer"),{code:"browser_bootstrap_unreadable"});
 assert.equal(f.cores[0].calls.some(x=>x[0]==="acceptBootstrap"),false);
 assert.equal(f.cores[0].freed,1);assert.equal(f.timers.size,0);
});

test("WebSocket constructor failure closes the attempt without retaining credentials",async()=>{
 const f=ioFixture();f.window.WebSocket=class{constructor(){throw new Error("credential secret");}};
 const client=f.makeClient();
 await assert.rejects(client.connect("observer"),e=>{assert.equal(e.message.includes("secret"),false);return true;});
 assert.equal(f.cores[0].freed,1);assert.equal(f.timers.size,0);assert.equal(client.closed,true);
});

test("an unopened socket is retired on Rust deadline expiry",async()=>{
 const f=ioFixture(available(),{tick:()=>{throw "browser_handshake_expired";}});const refused=[];
 const client=f.makeClient({onRefused:e=>refused.push(e.code)});
 const pending=client.connect("observer");const expired=assert.rejects(pending,{code:"browser_handshake_expired"});
 await nextTurn();f.tick(10200);await expired;
 assert.equal(f.sockets[0].closeCalls.length,1);assert.equal(f.cores[0].freed,1);
 assert.equal(f.timers.size,0);assert.deepEqual(refused,["browser_handshake_expired"]);
});

test("terminal lifecycle survives Rust-token retirement before JS teardown",async()=>{
 const f=ioFixture();const client=f.makeClient();await start(client,f);
 const lifecycle=f.cores[0].lifecycle.bind(f.cores[0]);
 f.cores[0].lifecycle=(token,event)=>{lifecycle(token,event);if(event==="pagehide")f.cores[0].token=null;};
 f.cores[0].disconnect=()=>{throw "browser_connection_cancelled";};
 client.lifecycle("pagehide");
 assert.equal(f.cores[0].freed,1);assert.equal(f.timers.size,0);assert.equal(f.sockets[0].closeCalls.length,1);
});
