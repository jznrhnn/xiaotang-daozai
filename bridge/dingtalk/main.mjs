import { DWClient, TOPIC_ROBOT } from 'dingtalk-stream';
import { createInterface } from 'node:readline';
import { spawn } from 'node:child_process';
import { createReceiver, fault, waitForStreamReady, sendNotification } from './protocol.mjs';
import { randomBytes } from 'node:crypto';
// SDK debug output must never print callback bodies, webhook URLs, tokens or credentials.
for (const method of ['log', 'info', 'warn', 'error', 'debug']) console[method] = () => {};
const emit = data => process.stdout.write(JSON.stringify(data) + '\n');
const finish = (data, status) => process.stdout.write(JSON.stringify(data) + '\n', () => process.exit(status));
let phase='components';let correlation=randomBytes(12).toString('hex');
const operation=process.argv.includes('--send')?'notify':'receive';
const diagnostic=row=>emit({diagnostic:{...row,correlation:row.correlation??correlation}});
async function step(stage, work) {
  phase=stage;const at=Date.now();diagnostic({operation,stage,outcome:'start',code:'OK'});
  try {const result=await work();diagnostic({operation,stage,outcome:'success',code:'OK',elapsed_ms:Date.now()-at});return result;}
  catch(error){diagnostic({operation,...fault(error,stage),outcome:'failure',elapsed_ms:Date.now()-at});throw error;}
}
const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
let initialized = false;
lines.on('line', line => {
  if (initialized) return;
  initialized = true;
  if (Buffer.byteLength(line) > 65536) { emit({ error: 'invalid initialization' }); process.exit(1); }
  let config;
  try { config = JSON.parse(line); } catch { emit({ error: 'invalid initialization' }); process.exit(1); }
  if (/^[a-f0-9]{24}$/.test(config.correlation??'')) correlation=config.correlation;
  start(config).catch(error => {
    const failure=fault(error,phase);
    finish({ error: '钉钉收发未完成，请检查网络、授权或连接冲突',failure },operation==='notify'?0:1);
  });
});
function helper(config, event) {
  return new Promise((resolve, reject) => {
    const child = spawn(config.exe, ['--dingtalk-event'], { windowsHide: true, stdio: ['pipe', 'pipe', 'ignore'], env: { ...process.env, DAOZAI_DINGTALK_BRIDGE: `${config.profile}|${config.robot}` } });
    let chunks = [], size = 0, finished = false;
    const finish = (err, value) => { if (finished) return; finished = true; clearTimeout(timer); err ? reject(err) : resolve(value); };
    const timer = setTimeout(() => { child.kill(); finish(Object.assign(Error(),{code:'HELPER_TIMEOUT'})); }, 45000);
    child.stdout.on('data', chunk => { size += chunk.length; if (size > 65536) { child.kill(); finish(Object.assign(Error(),{code:'HELPER_OUTPUT_LIMIT'})); } else { chunks.push(chunk); } });
    child.on('error', () => finish(Object.assign(Error(),{code:'HELPER_START_FAILED'})));
    child.on('close', code => { if (code !== 0) return finish(Object.assign(Error(),{code:'HELPER_EXIT_NONZERO'})); try { const result = JSON.parse(Buffer.concat(chunks).toString('utf8')); if (typeof result.reply !== 'string') throw Error(); finish(null, result.reply); } catch { finish(Object.assign(Error(),{code:'HELPER_INVALID_RESPONSE'})); } });
    child.stdin.on('error', () => {});
    child.stdin.end(JSON.stringify(event));
  });
}
async function start(config) {
  if (typeof config.clientId !== 'string' || config.clientId !== config.robot || typeof config.clientSecret !== 'string' || !config.clientSecret || typeof config.user !== 'string' || !config.user) throw Object.assign(Error(),{code:'INVALID_CREDENTIALS'});
  if (process.argv.includes('--send')) {
    const receipt=await sendNotification(config,{step});
    return finish({ receipt },0);
  }
  const client = new DWClient({ clientId: config.clientId, clientSecret: config.clientSecret, debug: false, keepAlive: true });
  client.config.autoReconnect = false;
  client.registerCallbackListener(TOPIC_ROBOT, createReceiver(config, {
    ack: id => client.socketCallBackResponse(id, { success: true }),
    helper: event => helper(config, event),
    diagnostic,
    onError: () => emit({ error: '消息处理或回复结果未确认；不会自动重试原任务，请核对客户端' }),
  }));
  await step('stream',async()=>{
    await client.connect();
    client.socket?.on('close',code=>diagnostic({operation,stage:'stream',outcome:'stopped',code:Number.isInteger(code)?`WS_CLOSE_${code}`:'WS_CLOSED'}));
    client.socket?.on('error',error=>{
      const status=/Unexpected server response: (\d{3})/.exec(error?.message??'');
      diagnostic({operation,stage:'stream',outcome:'failure',...fault(error,'stream'),...(status?{code:'WS_UPGRADE_REJECTED',http_status:Number(status[1])}:{})});
    });
    await waitForStreamReady(client);
  });
  emit({ ready: true });
  setInterval(() => { if (!client.connected || client.socket?.readyState !== 1) { diagnostic({operation,stage:'stream',outcome:'stopped',code:'STREAM_DISCONNECTED'});emit({ error: 'Stream 已断开，请重新连接或查看诊断日志' }); process.exit(1); } }, 3000);
  // Parent owns the stdin pipe and Windows job. Exit when it closes.
  lines.on('close', () => { client.disconnect(); process.exit(0); });
}
