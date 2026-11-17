// Adapted from soimy/openclaw-channel-dingtalk (MIT), commit
// 63956bc8ed9c168045a671271abb2ee1530c123d:
// src/messaging/quoted-ref.ts and src/messaging/send-service.ts.
// Copyright (c) 2026 YM Shen and contributors. See UPSTREAM-LICENSE.txt.
import { createHash } from 'node:crypto';
import { platformFetch } from './http.mjs';
const first = (...values) => values.find(v => typeof v === 'string' && v.trim())?.trim();
export function fault(error, stage) {
  const raw = error?.platformCode ?? error?.response?.data?.code ?? error?.response?.data?.errcode ?? error?.cause?.code ?? error?.code ?? error?.name;
  const code = String(raw ?? 'UNKNOWN');
  const status = Number(error?.httpStatus ?? error?.response?.status);
  return { stage, code: /^[A-Za-z0-9_.:-]{1,80}$/.test(code) ? code : 'UNKNOWN',
    http_status: Number.isInteger(status) && status >= 100 && status <= 599 ? status : undefined };
}
export async function waitForStreamReady(client, { timeout = 40000, now = Date.now, sleep = ms => new Promise(resolve => setTimeout(resolve, ms)) } = {}) {
  const started=now();
  // dingtalk-stream 2.1.4 resolves connect() when the socket is constructed,
  // before the async socket open. Current servers can deliver callbacks without
  // sending a separate REGISTERED event; do not require that optional SDK flag.
  while (!client.connected || client.socket?.readyState !== 1) {
    if (client.socket?.readyState === 3) throw Object.assign(Error(),{code:'STREAM_SOCKET_CLOSED'});
    if (now()-started>=timeout) throw Object.assign(Error(),{code:'STREAM_HANDSHAKE_TIMEOUT'});
    await sleep(100);
  }
}
export function quotedRef(data) {
  const replied = data.text?.repliedMsg;
  const content = data.content?.quoted;
  const receipt = first(data.originalProcessQueryKey, content?.processQueryKey);
  const message = first(replied?.msgId, data.originalMsgId, content?.msgId);
  const explicit = Boolean(receipt || message || replied || content || data.text?.isReplyMsg === true || data.isReply === true);
  return { explicit, receipt, message };
}
export function normalize(data, user) {
  if (String(data.conversationType) !== '1' || data.senderStaffId !== user) return null;
  const text = data.text?.content;
  if (typeof text !== 'string' || !text.trim() || Buffer.byteLength(text) > 16000) return null;
  if (typeof data.msgId !== 'string' || !data.msgId || typeof data.conversationId !== 'string' || !data.conversationId) return null;
  const timestamp = Number(data.createAt);
  if (!Number.isFinite(timestamp) || timestamp <= 0) return null;
  return { text: text.trim(), message: data.msgId, conversation: data.conversationId,
    at: Math.floor(timestamp > 1e12 ? timestamp / 1000 : timestamp), quote: quotedRef(data) };
}
export function markdown(text) { return { msgtype: 'markdown', markdown: { title: '岛仔', text } }; }
export function proactive(robot, user, text) {
  return { robotCode: robot, userIds: [user], msgKey: 'sampleMarkdown', msgParam: JSON.stringify({ title: '岛仔', text }) };
}
export function sessionUrl(raw) {
  const url = new URL(raw);
  if (url.protocol !== 'https:' || url.hostname !== 'oapi.dingtalk.com' || url.port || url.username || url.password || url.pathname !== '/robot/sendBySession') throw Error('invalid session endpoint');
  return url.href;
}
export async function request(url, body, headers = {}, fetcher = platformFetch) {
  const response = await fetcher(url, { method: 'POST', redirect: 'error', headers: { 'Content-Type': 'application/json', ...headers }, body: JSON.stringify(body), signal: AbortSignal.timeout(20000) });
  let value;
  try { value = await response.json(); } catch { const error = Error('invalid platform response'); error.platformCode = 'INVALID_RESPONSE'; error.httpStatus = response.status; throw error; }
  if (!value || typeof value !== 'object' || Array.isArray(value)) { const error=Error('invalid platform response');error.platformCode='INVALID_RESPONSE';error.httpStatus=response.status;throw error; }
  if (!response.ok || (value.errcode != null && value.errcode !== 0) || (value.code != null && value.code !== 0 && value.code !== '0')) {
    const error = Error('DingTalk request rejected');error.platformCode = value.code ?? value.errcode ?? 'HTTP_ERROR';error.httpStatus = response.status;throw error;
  }
  return value;
}
export async function replySession(webhook, text, fetcher = platformFetch) {
  const result = await request(sessionUrl(webhook), markdown(text), {}, fetcher);
  if (result.errcode !== 0) throw Error('session delivery not confirmed');
  return result;
}
export async function sendNotification(config,{requester=request,step=async(_,work)=>work()}={}){
  const token=await step('token',()=>requester('https://api.dingtalk.com/v1.0/oauth2/accessToken',{appKey:config.clientId,appSecret:config.clientSecret}));
  if(typeof token.accessToken!=='string'||!token.accessToken)throw Object.assign(Error(),{code:'MISSING_ACCESS_TOKEN'});
  const result=await step('notification',async()=>{
    const value=await requester('https://api.dingtalk.com/v1.0/robot/oToMessages/batchSend',proactive(config.robot,config.user,config.body),{'x-acs-dingtalk-access-token':token.accessToken});
    for(const [key,code] of [['invalidStaffIdList','INVALID_RECIPIENT'],['flowControlledStaffIdList','RECIPIENT_RATE_LIMITED']])if(Array.isArray(value[key])&&value[key].length)throw Object.assign(Error(),{code});
    if(!value.processQueryKey)throw Object.assign(Error(),{code:'MISSING_DELIVERY_RECEIPT'});
    return value;
  });
  return result.processQueryKey;
}
export function createReceiver(config, { ack, helper, reply = replySession, onError = () => {}, diagnostic = () => {}, now = Date.now }) {
  const seen = new Map(); let queue = Promise.resolve(); let pending = 0;
  const receive = async callback => {
    let correlation;
    const log = (outcome, code, stage = 'callback', more = {}) => diagnostic({ operation:'receive',stage,outcome,code,...more,correlation });
    if (callback.headers?.messageId) { try { ack(callback.headers.messageId); } catch (error) { log('failure','ACK_FAILED');onError(error);return; } }
    let data; try { if (typeof callback.data !== 'string' || Buffer.byteLength(callback.data) > 65536) { log('rejected','INVALID_CALLBACK');return; } data = JSON.parse(callback.data); } catch { log('rejected','INVALID_JSON');return; }
    const event = normalize(data, config.user);
    if (!event) { log('rejected',String(data.conversationType) !== '1' ? 'GROUP_IGNORED' : data.senderStaffId !== config.user ? 'USER_NOT_ALLOWED' : 'UNSUPPORTED_MESSAGE');return; }
    correlation=createHash('sha256').update(JSON.stringify([event.conversation,event.message])).digest('hex').slice(0,24);
    if (Math.abs(now() / 1000 - event.at) > 180) { log('rejected','STALE_MESSAGE');return; }
    const key = `${event.conversation}|${event.message}`;
    for (const [id, at] of seen) if (now() - at > 86400000) seen.delete(id);
    if (seen.has(key)) { log('duplicate','DUPLICATE_CALLBACK');return; }
    if (pending >= 10 || seen.size >= 2048) { log('rejected','QUEUE_LIMIT');return; }
    log('accepted','OK');
    seen.set(key, now()); pending++;
    queue = queue.then(async () => {
      let stage='helper';const started=now();
      try {
        sessionUrl(data.sessionWebhook);
        const result = await helper(event);log('success','OK',stage,{elapsed_ms:now()-started});
        stage='reply';const sent=now();await reply(data.sessionWebhook, result);log('success','OK',stage,{elapsed_ms:now()-sent});
      } catch(error) { log('failure','UNKNOWN',stage,{...fault(error,stage),elapsed_ms:now()-started});throw error; }
    }).catch(onError).finally(() => { pending--; });
  };
  receive.drain = () => queue;
  return receive;
}
