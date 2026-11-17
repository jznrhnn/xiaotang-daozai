import test from 'node:test';
import assert from 'node:assert/strict';
import { normalize, quotedRef, markdown, proactive, replySession, createReceiver, fault, waitForStreamReady, sendNotification } from './protocol.mjs';
import { createServer } from 'node:http';
const data = () => ({ msgId:'message', conversationId:'conversation', conversationType:'1', senderStaffId:'me', createAt:Date.now(), text:{content:'/tasks'} });
test('notification path uses token then exact bot/recipient and never repeats a rejected delivery',async()=>{
  const config={clientId:'bot',clientSecret:'synthetic',robot:'bot',user:'me',body:'private-body'};const calls=[];
  assert.equal(await sendNotification(config,{requester:async(url,body)=>{calls.push(url);if(url.endsWith('/accessToken'))return{accessToken:'token'};assert.equal(body.robotCode,'bot');assert.deepEqual(body.userIds,['me']);return{processQueryKey:'receipt'};}}),'receipt');assert.equal(calls.length,2);
  let attempts=0;await assert.rejects(sendNotification(config,{requester:async url=>{if(url.endsWith('/accessToken'))return{accessToken:'token'};attempts++;return{flowControlledStaffIdList:['me'],processQueryKey:'receipt'};}}),e=>e.code==='RECIPIENT_RATE_LIMITED');assert.equal(attempts,1);
});
test('actual Axios adapter preserves platform HTTP failure through a synthetic local server',async()=>{
  const server=createServer((req,res)=>{req.resume();req.on('end',()=>{res.writeHead(403,{'Content-Type':'application/json'});res.end(JSON.stringify({code:'Forbidden.NoPermission',message:'not-logged'}));});});
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  try{
    const {request}=await import('./protocol.mjs');let failure;
    try{await request(`http://127.0.0.1:${server.address().port}/mock`,{synthetic:true});}catch(error){failure=fault(error,'notification');}
    assert.equal(failure.http_status,403);assert.equal(failure.code,'Forbidden.NoPermission');
  }finally{await new Promise(resolve=>server.close(resolve));}
});
test('Stream readiness waits past three seconds for actual socket open without requiring REGISTERED',async()=>{
  const client={connected:false,registered:false,socket:{readyState:0}};let now=0;
  await waitForStreamReady(client,{now:()=>now,sleep:async ms=>{now+=ms;if(now>=3500){client.connected=true;client.socket.readyState=1;}if(now>=4200)client.registered=true;}});
  assert.equal(now,3500);assert.equal(client.registered,false);
});
test('Stream readiness rejects closed sockets and expires unopened handshakes',async()=>{
  await assert.rejects(waitForStreamReady({connected:false,registered:false,socket:{readyState:3}}),e=>e.code==='STREAM_SOCKET_CLOSED');
  let now=0;await assert.rejects(waitForStreamReady({connected:false,registered:false,socket:{readyState:0}},{timeout:500,now:()=>now,sleep:async ms=>{now+=ms;}}),e=>e.code==='STREAM_HANDSHAKE_TIMEOUT');
});
test('only the authorized user private messages reach the task router', () => {
  assert.equal(normalize(data(),'me').text,'/tasks');
  assert.equal(normalize(data(),'other'),null);
  assert.equal(normalize({...data(),conversationType:'2'},'me'),null);
  assert.equal(normalize({...data(),text:{content:'x'.repeat(16001)}},'me'),null);
  assert.equal(normalize({...data(),msgId:''},'me'),null);
  assert.equal(normalize({...data(),createAt:'invalid'},'me'),null);
});
test('quotes retain original receipt and message id without looking at rendered text', () => {
  const ref=quotedRef({...data(), originalProcessQueryKey:'receipt', text:{content:'继续',isReplyMsg:true,repliedMsg:{msgId:'original',content:{text:'partial'}}}});
  assert.deepEqual(ref,{explicit:true,receipt:'receipt',message:'original'});
  assert.equal(quotedRef({text:{isReplyMsg:true}}).explicit,true);
  assert.equal(quotedRef(data()).explicit,false);
});
test('short session replies are sent as actual Markdown without padding',async () => {
  const body='- asd\n- asd';let sent;
  await replySession('https://oapi.dingtalk.com/robot/sendBySession?session=synthetic',body,async(url,options)=>{
    sent=JSON.parse(options.body);assert.equal(options.redirect,'error');return {ok:true,json:async()=>({errcode:0})};
  });
  assert.deepEqual(sent,markdown(body));assert.equal(sent.msgtype,'markdown');assert.equal(sent.markdown.text,body);
});
test('session URLs cannot redirect replies to an arbitrary endpoint',async () => {
  for(const url of ['http://oapi.dingtalk.com/robot/sendBySession','https://attacker.test/robot/sendBySession','https://oapi.dingtalk.com@attacker.test/robot/sendBySession','https://oapi.dingtalk.com/other']){
    await assert.rejects(replySession(url,'body',()=>{assert.fail('must not send');}));
  }
});
test('platform business errors and HTTP errors are not successful replies',async () => {
  const url='https://oapi.dingtalk.com/robot/sendBySession';
  await assert.rejects(replySession(url,'body',async()=>({ok:true,json:async()=>({errcode:400})})));
  await assert.rejects(replySession(url,'body',async()=>({ok:false})));
});
test('proactive notification keeps body and selected bot/recipient exact',()=>{
  const payload=proactive('robot','me','**结果**');assert.equal(payload.msgKey,'sampleMarkdown');assert.deepEqual(payload.userIds,['me']);
  assert.equal(payload.robotCode,'robot');assert.equal(JSON.parse(payload.msgParam).text,'**结果**');
});
test('actual callback pipeline acks duplicates but dispatches once and replies Markdown',async()=>{
  let dispatched=0,acks=0,sent;
  const receive=createReceiver({user:'me'},{ack:()=>{acks++;},helper:async event=>{dispatched++;assert.equal(event.text,'/tasks');return '- asd\n- asd';},reply:(url,body)=>replySession(url,body,async(_,options)=>{sent=JSON.parse(options.body);return {ok:true,json:async()=>({errcode:0})};})});
  const callback={headers:{messageId:'callback'},data:JSON.stringify({...data(),sessionWebhook:'https://oapi.dingtalk.com/robot/sendBySession?session=synthetic'})};
  await receive(callback);await receive(callback);await receive({...callback,data:JSON.stringify({...data(),senderStaffId:'other'})});await receive.drain();
  assert.equal(acks,3);assert.equal(dispatched,1);assert.equal(sent.msgtype,'markdown');assert.equal(sent.markdown.text,'- asd\n- asd');
});
test('uncertain helper outcomes are never replayed and invalid session URLs never dispatch',async()=>{
  let calls=0,errors=0;
  const receive=createReceiver({user:'me'},{ack:()=>{},helper:async()=>{calls++;throw Error('uncertain');},onError:()=>{errors++;}});
  const callback={headers:{messageId:'callback'},data:JSON.stringify({...data(),sessionWebhook:'https://oapi.dingtalk.com/robot/sendBySession'})};
  await receive(callback);await receive.drain();await receive(callback);await receive.drain();
  await receive({...callback,data:JSON.stringify({...data(),msgId:'invalid-endpoint',sessionWebhook:'https://attacker.test/robot/sendBySession'})});await receive.drain();
  assert.equal(calls,1);assert.equal(errors,2);
});
test('HTTP status and platform code survive without leaking response messages or secrets',async()=>{
  let error;
  try { await replySession('https://oapi.dingtalk.com/robot/sendBySession','private-body',async()=>({ok:false,status:403,json:async()=>({code:'Forbidden.NoPermission',message:'private-token',clientSecret:'private-secret'})})); } catch(e) { error=e; }
  const detail=fault(error,'reply');assert.equal(detail.code,'Forbidden.NoPermission');assert.equal(detail.http_status,403);
  const raw=JSON.stringify(detail);for(const value of ['private-body','private-token','private-secret'])assert.ok(!raw.includes(value));
  assert.equal(fault({message:'private-secret',code:'https://host?token=secret'},'reply').code,'UNKNOWN');
  assert.equal(fault({cause:{code:'ETIMEDOUT'}},'reply').code,'ETIMEDOUT');
});
test('callback diagnostics explain filtering, duplication and delivery rejection without body logging',async()=>{
  const logs=[];const receive=createReceiver({user:'me'},{ack:()=>{},helper:async()=>'- private-body',diagnostic:row=>logs.push(row),onError:()=>{},reply:async()=>{throw Object.assign(Error('private-token'),{platformCode:'RATE_LIMITED',httpStatus:429});}});
  const callback={headers:{messageId:'private-callback'},data:JSON.stringify({...data(),sessionWebhook:'https://oapi.dingtalk.com/robot/sendBySession?session=private-secret'})};
  await receive(callback);await receive.drain();await receive(callback);
  await receive({...callback,data:JSON.stringify({...data(),senderStaffId:'another-user'})});
  await receive({...callback,data:'not-json'});
  assert.ok(logs.some(row=>row.code==='RATE_LIMITED'&&row.http_status===429));assert.ok(logs.some(row=>row.code==='DUPLICATE_CALLBACK'));assert.ok(logs.some(row=>row.code==='USER_NOT_ALLOWED'));assert.ok(logs.some(row=>row.code==='INVALID_JSON'));
  const raw=JSON.stringify(logs);for(const value of ['private-body','private-token','private-secret','private-callback','another-user'])assert.ok(!raw.includes(value));
});
