// Same HTTP client family as soimy/openclaw-channel-dingtalk's send-service.
import axios from 'axios';
const client=axios.create({timeout:20000,maxRedirects:0,validateStatus:()=>true});
export async function platformFetch(url,options){
  const response=await client.request({url,method:options.method,headers:options.headers,data:options.body,signal:options.signal});
  return {ok:response.status>=200&&response.status<300,status:response.status,json:async()=>response.data};
}
