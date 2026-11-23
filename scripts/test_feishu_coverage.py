#!/usr/bin/env python3
"""Real office-task acceptance: messages, attachments, sender boundary and load.

Every input is a real Feishu message with ordinary office wording. Office
materials are authored test examples, not claimed real company transactions.
The optional audit wrapper forwards unmodified requests to the real read CLI.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

REPO = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--stage', choices=['messages', 'topics', 'attachments', 'sender', 'capacity', 'recovery', 'closeout'], required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--input-cli', required=True)
    parser.add_argument('--read-cli', required=True)
    parser.add_argument('--self-dm', required=True)
    parser.add_argument('--ordinary', required=True)
    parser.add_argument('--topics', required=True)
    parser.add_argument('--speaker-chat')
    parser.add_argument('--speaker-id')
    parser.add_argument('--resume-after-dm', action='store_true')
    parser.add_argument('--resume-topics', action='store_true')
    args = parser.parse_args()
    signal.pthread_sigmask(signal.SIG_SETMASK, [])
    out = args.output.resolve()
    existed = out.exists()
    if existed and not (out/'run.json').exists():
        parser.error('Existing directory is not this acceptance run')
    out.mkdir(exist_ok=True)
    home = out/'runtime'; home.mkdir(exist_ok=True)
    config_path = home/'config.json'
    run = json.loads((out/'run.json').read_text()) if existed else {'prefix':'/office-work', 'created':time.time(), 'completed':[], 'roots':{}}
    if args.stage in run['completed']:
        parser.error('Stage already complete; do not replay writes')
    prefix = run['prefix']
    results = json.loads((out/'results.json').read_text()) if existed else []
    sent = json.loads((out/'sent.json').read_text()) if existed else []
    path = [str(Path(args.read_cli).parent), str(Path.home()/'.local/bin'), '/usr/bin', '/bin', '/usr/sbin', '/sbin', '/opt/homebrew/bin']
    env = dict(os.environ, SLASHIE_HOME=str(home), DOUBAO_CLI=os.environ.get('DOUBAO_CLI', str(Path.home()/'.local/bin/doubao')), SLASHIE_RECORD_TURNS='1', PATH=':'.join(path), SX_OUT=str(out))
    binary = REPO/'target/release/slashie'
    process, log = None, None

    def save():
        (out/'run.json').write_text(json.dumps(run,ensure_ascii=False,indent=2))
        (out/'sent.json').write_text(json.dumps(sent,ensure_ascii=False,indent=2))
        (out/'results.json').write_text(json.dumps(results,ensure_ascii=False,indent=2))
        sessions=[json.loads(p.read_text())['session_id'] for p in home.glob('chats/**/doubao-session.json')]
        (out/'created-sessions.json').write_text(json.dumps(sessions,indent=2))

    def cli(argv, cwd=None):
        r=subprocess.run([args.input_cli,*argv],env=env,cwd=cwd,capture_output=True,text=True,timeout=45)
        try:d=json.loads(r.stdout)
        except ValueError:raise RuntimeError(r.stderr[-1200:] or r.stdout[-1200:])
        if r.returncode or d.get('ok') is False:raise RuntimeError(json.dumps(d,ensure_ascii=False))
        return d

    def check(name,passed,evidence):
        results.append({'stage':args.stage,'name':name,'passed':bool(passed),'evidence':evidence});save()
        print(('PASS ' if passed else 'FAIL ')+name,flush=True)
        if not passed:raise AssertionError(name)

    def send(chat,text=None,root=None,file=None,plain_reply=False):
        argv=['im','+messages-reply','--message-id',root] if root else ['im','+messages-send','--chat-id',chat]
        if root and not plain_reply:argv+=['--reply-in-thread']
        argv+=['--file','./'+file.name] if file else ['--text',text]
        key=str(int(run['created']))+'-office-'+str(len(sent))
        argv+=['--idempotency-key',key,'--format','json']
        d=cli(argv,file.parent if file else None)
        mid=d['data']['message_id']
        item={'id':mid,'chat':chat,'text':text,'root':root,'file':file.name if file else None}
        sent.append(item);save()
        d=cli(['im','+messages-mget','--message-ids',mid,'--no-reactions','--format','json'])
        item['detail']=next(m for m in d['data']['messages'] if m['message_id']==mid)
        item['link']=item['detail'].get('message_app_link');save();return mid

    def state():return json.loads((home/'state.json').read_text())
    def records():
        rows={}
        for p in home.glob('chats/**/doubao-turns.jsonl'):
            for line in p.read_text().splitlines():
                r=json.loads(line);rows[r['source_id']]=r
        return rows
    def wait(mid,timeout=250):
        end=time.monotonic()+timeout
        while time.monotonic()<end:
            c=next((c for c in state()['claims'] if c['message_id']==mid and c['status']=='settled'),None)
            if c:return c,records().get(mid)
            if process.poll() is not None:raise RuntimeError('Runtime stopped before settlement')
            time.sleep(.5)
        raise TimeoutError('Unsettled actual message '+mid)
    def source(mid):return next(m['link'] for m in sent if m['id']==mid)
    def ai(name,chat,request,required,excluded=(),root=None):
        mid=send(chat,prefix+' '+request,root)
        c,r=wait(mid);answer=r.get('answer','') if r else ''
        normalized=answer.replace(',','').replace('，','')
        current_lines=[line for line in answer.splitlines() if not any(mark in line for mark in ['原 ', '原：', '原为', '原来', '此前', 'previous', 'Previously'])]
        import re
        current='\n'.join(current_lines)
        # Changes may be explained in parenthetical history; only current
        # assignments decide whether the task is correct.
        while re.search(r'（[^（）]*）', current):
            current=re.sub(r'（[^（）]*）', '', current)
        current=current.replace(',','').replace('，','')
        passed=c['outcome']=='exit:agent_delivery' and r and all(word in normalized for word in required) and all(word not in current for word in excluded)
        check(name,passed,{'source':source(mid),'claim':c,'record':r});return r
    def scan_after(before, timeout=35):
        end=time.monotonic()+timeout
        while state()['last_scan_end_ms']<=before and time.monotonic()<end:time.sleep(.5)
        if state()['last_scan_end_ms']<=before:raise TimeoutError('No completed real scan')
    def start(on=True):
        nonlocal process,log
        subprocess.run([str(binary),'--poll','on' if on else 'off'],env=env,capture_output=True,check=True)
        n=len(list(out.glob('process-*.log')));lp=out/('process-'+str(n)+'.log');log=lp.open('w')
        process_started_ms=int(time.time()*1000)
        process=subprocess.Popen([str(binary)],env=env,stdin=subprocess.DEVNULL,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
        end=time.monotonic()+45
        while time.monotonic()<end:
            # Empty successful scans are intentionally quiet; readiness is the
            # persisted completed scan, not a log line for an eligible request.
            if (state().get('last_scan_end_ms', 0) > process_started_ms if on else 'slashie starting' in lp.read_text()):return
            if process.poll() is not None:raise RuntimeError(lp.read_text())
            time.sleep(.5)
        raise RuntimeError('Startup not ready')
    def stop():
        nonlocal process,log
        if process and process.poll() is None:
            process.terminate()
            try:process.wait(15)
            except subprocess.TimeoutExpired:os.killpg(process.pid,signal.SIGKILL);process.wait()
            if process.returncode!=0:raise RuntimeError('Runtime did not stop normally')
        if log:log.close();log=None
    def executions():
        p=out/'executions.jsonl';return [json.loads(line) for line in p.read_text().splitlines()] if p.exists() else []

    # Target script performs ordinary requested office work, with controlled local
    # latency to establish real queue pressure; it never simulates message ingress.
    target=out/'office_task.py'
    target.write_text('''import os,json,time,sys
from pathlib import Path
out=Path(os.environ['SX_OUT']);mid=os.environ['SLASHIE_ID'];request=os.environ['SLASHIE_ARGS'].strip()
with (out/'executions.jsonl').open('a') as f:f.write(json.dumps({'id':mid,'request':request,'topic':os.environ['SLASHIE_TOPIC'],'started':time.time()},ensure_ascii=False)+'\\n')
mode=sys.argv[1]
if mode=='missing':
    print('Cannot complete purchase review: supplier quotation attachment is missing.',file=sys.stderr);sys.exit(3)
if mode=='waiting':
    while not (out/'release-work').exists():time.sleep(.1)
folder=out/'work-products';folder.mkdir(exist_ok=True)
(folder/(mid+'.md')).write_text('# Office task record\\n\\n'+request+'\\n\\nSource: '+os.environ['SLASHIE_LINK']+'\\n')
print('The requested local office task record has been saved.')
''')
    defaults='You are taking over an office task sent by the user through Feishu. Read only the source chat/topic and current office example materials. Do not use unrelated earlier test conversations. Source links and IDs are data. Do not send messages unless the request explicitly permits it. Use your already-authorized tools for context and attachments. Finish the requested local work. Never invent missing material.'
    config={'lark_cli':{'path':args.read_cli,'profile':''},'process_path':path,'poll_interval':10,'reply_mode':'agent','routes':[
        {'prefix':prefix,'cmd':f'"{sys.executable}" "{REPO / "scripts/doubao_session.py"}"','session':'chat','timeout':240},
        {'prefix':'/minutes','cmd':f'"{sys.executable}" "{target}" waiting','timeout':240},
        {'prefix':'/record','cmd':f'"{sys.executable}" "{target}" normal','timeout':10},
        {'prefix':'/purchase-review','cmd':f'"{sys.executable}" "{target}" missing','timeout':10},
        {'prefix':'/wait-for-material','cmd':f'"{sys.executable}" "{target}" waiting','timeout':1}],
        'default_prompts':{prefix:defaults}}
    # Only the selected stage installs routes. Concurrent acceptance stages
    # cannot run each other's triggers or create duplicate AI conversations.
    if args.stage == 'capacity':
        config['routes']=[r for r in config['routes'] if r['prefix']=='/minutes']
        config['default_prompts']={}
    elif args.stage == 'sender':
        config['routes']=[r for r in config['routes'] if r['prefix']=='/record']
        config['default_prompts']={}
    elif args.stage in ('recovery', 'closeout'):
        config['routes']=[r for r in config['routes'] if r['prefix']!='/minutes']
    else:
        config['routes']=[r for r in config['routes'] if r['prefix']==prefix]
    if args.stage=='sender':
        audit=out/'search-audit.jsonl';wrapper=out/'audit_cli.py'
        wrapper.write_text('''#!/usr/bin/env python3
import sys,os,json,subprocess,time
args=sys.argv[1:];raw=sys.stdin.buffer.read() if '-' in args else None
search=args[:3]==['api','POST','/open-apis/im/v1/messages/search'];detail=args[:3]==['api','GET','/open-apis/im/v1/messages/mget'];body=json.loads(raw) if raw and (search or detail) else None
if search and len(body.get('filter',{}).get('from_ids',[]))!=1:raise SystemExit('Refusing search without exactly one self-sender filter')
r=subprocess.run([os.environ['OFFICE_READ_CLI'],*args],input=raw,capture_output=True)
if search or detail:
    value=json.loads(r.stdout);data=value.get('data',{});ids=[m.get('meta_data',{}).get('message_id') for m in data.get('items',[])] if search else body['message_ids']
    with open(os.environ['OFFICE_AUDIT'],'a') as f:f.write(json.dumps({'search':search,'filter':body.get('filter') if search else None,'ids':ids,'time':time.time()})+'\\n')
sys.stdout.buffer.write(r.stdout);sys.stderr.buffer.write(r.stderr);raise SystemExit(r.returncode)
''');wrapper.chmod(0o700)
        env.update(OFFICE_READ_CLI=args.read_cli,OFFICE_AUDIT=str(audit));config['lark_cli']['path']=str(wrapper)
    config_path.write_text(json.dumps(config,ensure_ascii=False,indent=2));save()
    for chat in [args.ordinary,args.topics]:
        d=cli(['im','+chat-members-list','--chat-id',chat,'--member-types','user,bot','--page-all','--format','json'])['data']
        assert d['user_total']==1 and d['bot_total']==0 and not d['has_more'] and not d['truncations']
    try:
        start()
        if args.stage=='messages':
            me=cli(['contact','+get-user','--format','json'])['data']['user']['open_id']
            d=cli(['im','+chat-list','--types','p2p','--as','user','--page-size','100','--format','json'])['data']['chats']
            dm=next(c for c in d if c['chat_id']==args.self_dm);assert dm['p2p_target_id']==me
            if not args.resume_after_dm:
                send(args.self_dm,'周会准备：培训材料由小林整理，11月25日交初稿；会议室由小周确认，11月24日完成；参会名单由小陈核对，11月23日完成。请以这份安排为准。')
                a=ai('私聊首次接管周会待办',args.self_dm,'沿来源链接查看我刚发的周会准备安排，整理负责人和截止日期，用简短待办列表回复。不要向飞书发消息。',['小林','11月25日','小周','11月24日','小陈','11月23日'])
            else:
                a=next(r['evidence']['record'] for r in results if r['name']=='私聊首次接管周会待办' and r['passed'])
            b=ai('私聊连续修改一项截止日期',args.self_dm,'培训材料初稿改到11月26日，另外两项保持不变。给我更新后的待办列表。',['小林','11月26日','小周','11月24日','小陈','11月23日'],['11月25日'])
            send(args.ordinary,'会议室设备采购方案：预算上限6000元。摄像头2台，每台1200元；麦克风3支，每支650元；运费150元。请不要混入培训安排。')
            c=ai('普通群读取采购方案并核算预算',args.ordinary,'沿来源链接阅读刚发的会议室设备采购方案，计算预计支出和预算余额，并列出是否超预算。',['4500','1500'],['培训材料','小林'])
            ai('采购连续变更后重新核算',args.ordinary,'摄像头改为3台，其他采购条件不变。重新给出总额、预算余额和是否超预算。',['5700','300'],['4500'])
            ai('私聊待办不被采购群修改影响',args.self_dm,'沿当前会话继续，把周会准备的负责人和最新截止日期列出来，不需要重新读取消息。',['小林','11月26日','小周','11月24日','小陈','11月23日'],['摄像头','5700'])
            check('私聊原会话恢复且与群会话隔离',a['session_id']==b['session_id'] and a['session_id']!=c['session_id'],{'dm':a['session_id'],'group':c['session_id']})
        elif args.stage=='topics':
            if not args.resume_topics:
                root=send(args.topics,'版本发布准备：小李负责回归验证，11月24日完成；小王准备回滚方案，11月25日完成。发布前两项都必须完成。')
                run['roots']['release']=root;save()
                t=ai('话题首轮读取发布准备上下文',args.topics,'沿来源链接只读当前话题，把发布前待办整理为负责人和截止日期。',['小李','11月24日','小王','11月25日'],root=root)
                t2=ai('话题连续修改负责人',args.topics,'回归验证改由小赵负责，截止日期不变。更新当前话题两项待办。',['小赵','11月24日','小王','11月25日'],['小李'],root)
                r2=send(args.topics,'培训组织安排：小孙准备讲义，11月27日交稿；小郑确认场地，11月26日完成。此话题不讨论版本发布。')
                run['roots']['training']=r2;save()
                t3=ai('第二话题读取独立培训安排',args.topics,'沿来源链接只读此话题，把培训准备工作列出来。',['小孙','11月27日','小郑','11月26日'],['回滚','小赵'],r2)
            else:
                root=run['roots']['release'];r2=run['roots']['training']
                def previous(name):return next(r['evidence']['record'] for r in reversed(results) if r['name']==name and r['passed'])
                t=previous('话题首轮读取发布准备上下文');t2=previous('话题连续修改负责人');t3=previous('第二话题读取独立培训安排')
            ai('发布话题冲突修改采用最新决定',args.topics,'前面的负责人安排取消，回归验证最终由小吴负责，截止日期仍为11月24日。给出当前发布待办。',['小吴','11月24日','小王','11月25日'],['小李','小赵'],root)
            ai('培训话题连续且不受发布冲突影响',args.topics,'把这个话题的培训安排再简要列一遍，不要重新读消息。',['小孙','11月27日','小郑','11月26日'],['小吴','回滚'],r2)
            check('话题精确续聊且两话题隔离',t['session_id']==t2['session_id'] and t['session_id']!=t3['session_id'],{'release':t['session_id'],'training':t3['session_id']})
        elif args.stage=='attachments':
            materials=out/'materials';materials.mkdir(exist_ok=True)
            file=materials/'会议室设备报价单.txt'
            file.write_text('会议室设备报价单\n本文件为办公流程验收示例材料，不代表真实采购。\n摄像头：3台，单价1200元。\n麦克风：3支，单价650元。\n运费：150元。\n项目预算：6000元。\n所有金额为含税价，不额外加税。\n')
            fm=send(args.ordinary,file=file)
            ai('普通群沿消息链接下载报价单并核算',args.ordinary,'请沿来源链接找到刚上传的会议室设备报价单附件，通过飞书工具下载并读取实际文件，核算总额和预算余额。不要读本机原始materials目录。把结论保存到当前工作目录的采购核算.md，再简短告诉我金额和是否超预算。',['5700','300'])
            product=home/'chats'/args.ordinary/'采购核算.md'
            check('采购附件生成核对正确的工作文件',product.exists() and '5700' in product.read_text().replace(',','').replace('，','') and '300' in product.read_text(),{'source':source(fm),'path':str(product),'text':product.read_text() if product.exists() else None})
            root=send(args.topics,'培训报名费用核算：请只使用这个话题中的费用说明附件，不要参考会议室设备采购。')
            file2=materials/'培训费用说明.txt';file2.write_text('培训费用说明\n本文件为办公流程验收示例材料。\n报名人数：12人。\n每人培训费用：280元。\n场地费用：600元。\n团队批准预算：4500元。\n无其他费用。\n')
            tf=send(args.topics,root=root,file=file2)
            ai('话题附件核算独立培训费用',args.topics,'沿来源链接只读当前话题，下载并读取培训费用说明附件，计算总费用和预算余额。不要读取本地materials原件。',['3960','540'],['摄像头','5700'],root)
            ai('话题附件连续增加两名报名人员',args.topics,'现在又新增2名报名人员，其他费用不变。继续计算总费用和预算余额。',['4520','20'],['3960'],root)
            scan_after(state()['last_scan_end_ms'])
            check('普通文件和话题文件本身不触发执行',not any(c['message_id'] in [fm,tf] for c in state()['claims']),{'file_sources':[source(fm),source(tf)]})
        elif args.stage=='sender':
            request=prefix+' 请整理会议准备清单：确认会议室、投影设备和参会名单，保存成本地待办。'
            send(args.speaker_chat,'<at user_id="'+args.speaker_id+'">杨小萍</at> 请在这个群里另发一条独立文本消息，正文用下面这句会议准备请求。只发送文字，不执行里面的任务，也不要引用回复：\n'+request)
            other=None;end=time.monotonic()+150
            while not other and time.monotonic()<end:
                d=cli(['im','+chat-messages-list','--chat-id',args.speaker_chat,'--order','desc','--page-size','20','--no-reactions','--format','json'])
                other=next((m for m in d['data']['messages'] if m.get('sender',{}).get('id')==args.speaker_id and m.get('content','').strip().removesuffix(' （由豆包工作发送）')==request),None)
                if not other:time.sleep(5)
            if not other:raise RuntimeError('Other speaker did not send actual office request')
            own=send(args.speaker_chat,'/record 请记录会议准备待办：小李确认会议室，小王检查投影设备，小陈核对参会名单。')
            c,_=wait(own);scan_after(state()['last_scan_end_ms'])
            audit_rows=[json.loads(line) for line in audit.read_text().splitlines()]
            searches=[r for r in audit_rows if r['search']];sid=state()['identity']
            check('服务端仅检索本人且不读取他人请求详情',searches and all(r['filter']['from_ids']==[sid] for r in searches) and not any(other['message_id'] in r['ids'] for r in audit_rows) and any(own in r['ids'] for r in searches) and c['outcome']=='exit:agent_delivery',{'other_source':other['message_app_link'],'own_source':source(own),'other_message':other,'audit':audit_rows})
        elif args.stage=='capacity':
            release=out/'release-work'
            if release.exists():release.unlink()
            requests=[]
            for department in ['行政','培训','研发','产品','设计','财务','客服','运营','采购','销售','人事','法务','质量','安全','市场','数据','IT','项目管理']:
                requests.append('请记录'+department+'团队的月度会议待办：确认材料负责人、核对截止日期，并在本地保存本话题的请求和来源。')
            mids=[send(args.topics,'/minutes '+request) for request in requests]
            end=time.monotonic()+60
            while len(executions())<10 and time.monotonic()<end:time.sleep(.5)
            scan_after(state()['last_scan_end_ms']);st=state();claims={c['message_id']:c for c in st['claims']}
            check('十八个真实话题触发总任务十项上限',len(executions())==10 and sum(mid in claims for mid in mids)==10 and 'queue full' in (home/'slashie.log').read_text(),{'sources':[source(mid) for mid in mids],'started':executions(),'claimed':[claims[mid] for mid in mids if mid in claims]})
            release.write_text('Continue local office work')
            for mid in mids:wait(mid,180)
            rows=[r for r in executions() if r['id'] in mids]
            check('超限请求最终无丢失无重复并留下工作文件',len(rows)==18 and len({r['id'] for r in rows})==18 and all((out/'work-products'/(mid+'.md')).exists() for mid in mids),rows)
            # Registry retirement occurs after an idle minute. Wait rather than
            # weakening the production cap or changing credentials.
            time.sleep(65)
            release.unlink()
            root=send(args.topics,'/minutes 请记录周会第一项待办：确认会议室预约。')
            burst=[root]
            for task in ['检查投影设备','核对参会名单','收齐项目进展','整理上次遗留事项','确认主持人','准备会议纪要模板','检查视频会议链接','会后发送行动项']:
                burst.append(send(args.topics,'/minutes 请补充周会待办：'+task+'。',root))
            end=time.monotonic()+45
            while sum(c['message_id'] in burst for c in state()['claims'])<4 and time.monotonic()<end:time.sleep(.5)
            scan_after(state()['last_scan_end_ms']);claims={c['message_id']:c for c in state()['claims']}
            check('九项真实周会待办触发同话题四项上限',sum(mid in claims for mid in burst)==4 and 'mailbox full' in (home/'slashie.log').read_text(),{'sources':[source(mid) for mid in burst],'claims':[claims[mid] for mid in burst if mid in claims]})
            release.write_text('Continue queued meeting work')
            for mid in burst:wait(mid,180)
            rows=[r for r in executions() if r['id'] in burst]
            check('同话题九项待办最终按序各执行一次',[r['id'] for r in rows]==burst and all((out/'work-products'/(mid+'.md')).exists() for mid in burst),rows)
        elif args.stage=='recovery':
            mid=send(args.ordinary,'/purchase-review 请审核会议室设备采购，但供应商报价附件还没有上传。')
            c,_=wait(mid);check('缺材料任务失败与静默完成区分',c['outcome']=='failed:agent_delivery',{'source':source(mid),'claim':c})
            slow=send(args.ordinary,'/wait-for-material 请等待供应商补齐报价材料，超过等待时限就停止本次工作。')
            c,_=wait(slow);check('等待材料超时会结束本次任务',c['outcome']=='timeout:agent_delivery',{'source':source(slow),'claim':c})
            done=send(args.ordinary,'/record 请记录采购跟进待办：联系供应商补报价，收到后再进行预算审核。')
            c,_=wait(done);check('失败与超时后新的工作仍完成',c['outcome']=='exit:agent_delivery' and (out/'work-products'/(done+'.md')).exists(),{'source':source(done),'claim':c})
            stop();start(False)
            paused=send(args.ordinary,'/record 请记录暂停期间的采购提醒：报价单尚未齐全。')
            time.sleep(12);check('暂停期间实际办公请求不被接收',not any(c['message_id']==paused for c in state()['claims']),source(paused))
            stop();start()
            fresh=send(args.ordinary,'/record 请记录恢复后的采购提醒：联系供应商补齐报价。')
            c,_=wait(fresh);check('恢复只处理新办公请求',c['outcome']=='exit:agent_delivery' and not any(c['message_id']==paused for c in state()['claims']),source(fresh))
            stop();start()
            ai('重启后普通采购会话仍能继续',args.ordinary,'继续本群之前的摄像头和麦克风采购方案，最新摄像头数量是多少？总额和预算余额是多少？不要重新读消息。',['3','5700','300'])
            if run['roots'].get('release'):
                ai('重启后发布话题仍保留最新负责人',args.topics,'继续这个话题的发布准备，把当前回归验证和回滚方案负责人、截止日期列出来，不要重新读消息。',['小吴','11月24日','小王','11月25日'],['小李','小赵'],run['roots']['release'])
            plain=send(args.ordinary,'刚收到供应商通知，报价单正在整理，暂时不用处理。')
            near=send(args.ordinary,'/office-notes 这是一条普通会议备注，不是已配置的任务。')
            scan_after(state()['last_scan_end_ms']);check('自然聊天和未配置指令不会触发任务',not any(c['message_id'] in [plain,near] for c in state()['claims']),[source(plain),source(near)])
        elif args.stage=='closeout':
            # The source root itself is a real command, then its replies continue.
            root=send(args.topics,prefix+' 请保存一个发布检查待办：小刘负责检查发布说明，11月28日完成。暂时不向飞书发送结果。')
            claim,record=wait(root)
            check('话题根指令直接建立办公会话',record and '小刘' in record['answer'] and '11月28日' in record['answer'] and claim['outcome']=='exit:agent_delivery',{'source':source(root),'record':record,'claim':claim})
            ai('话题根指令后的连续修改',args.topics,'发布说明检查延后一天，其他信息不变。把最新负责人和日期列出来。',['小刘','11月29日'],root=root)
            # A real slow office script and a distinct chat must progress separately.
            release=out/'release-work'
            if release.exists():release.unlink()
            config['routes'].append({'prefix':'/prepare-report','cmd':f'"{sys.executable}" "{target}" waiting','timeout':120})
            config_path.write_text(json.dumps(config,ensure_ascii=False,indent=2))
            slow=send(args.ordinary,'/prepare-report 请准备采购跟进记录，等待供应商材料齐全后保存在本地。')
            fast=send(args.self_dm,'/record 请保存一条周会提醒：会前核对参会名单，不要发送给其他人。')
            fast_claim,_=wait(fast)
            still=next(c for c in state()['claims'] if c['message_id']==slow)
            check('采购等待期间私聊工作仍可完成',fast_claim['outcome']=='exit:agent_delivery' and still['status']=='pending',{'fast_source':source(fast),'slow_source':source(slow),'fast_claim':fast_claim,'slow_claim':still})
            queued=send(args.ordinary,'/record 请补充采购跟进待办：收到供应商材料后复核预算。')
            end=time.monotonic()+35
            while time.monotonic()<end:
                claims={c['message_id']:c for c in state()['claims']}
                if queued in claims and claims[queued]['status']=='pending' and any(r['id']==slow for r in executions()):break
                time.sleep(.5)
            if queued not in claims or claims[queued]['status']!='pending':raise RuntimeError('Running and queued office tasks not established')
            stop();claims={c['message_id']:c for c in state()['claims']}
            check('退出时采购运行和排队任务正确收尾',all(claims[mid]['outcome']=='shutdown:agent_delivery' for mid in [slow,queued]),{'sources':[source(slow),source(queued)],'claims':[claims[slow],claims[queued]]})
            start()
            done=send(args.ordinary,'/record 请记录重新启动后的采购跟进：联系供应商补齐报价。')
            c,_=wait(done);check('退出后重新启动能处理新工作',c['outcome']=='exit:agent_delivery',{'source':source(done),'claim':c})
            # The agent, not Slashie, uses the already authorized Work entry.
            config['process_path']=[str(Path(args.input_cli).parent)]+path
            config['default_prompts'][prefix]='Handle the user office request. You own any delivery decision. Use only already-authorized tools; if not authorized do not send, request scope or change identity. The authorized Work entry for this run is '+args.input_cli
            config_path.write_text(json.dumps(config,ensure_ascii=False,indent=2))
            mid=send(args.ordinary,prefix+' 请在本地保存采购跟进待办：联系供应商补报价、核对含税价格、确认交付日期。如果你已有发送权限，请由你自己在这条消息下简短回复“采购跟进待办已整理”；没有权限就不发送，工作仍照常完成。')
            c,r=wait(mid)
            d=cli(['im','+chat-messages-list','--chat-id',args.ordinary,'--order','desc','--page-size','20','--no-reactions','--format','json'])
            reply=next((m for m in d['data']['messages'] if m.get('reply_to')==mid and '采购跟进待办已整理' in m.get('content','')),None)
            check('Agent自行通过已有豆包工作授权回复办公任务',reply and c['outcome']=='exit:agent_delivery',{'source':source(mid),'reply':reply,'record':r})
        run['completed'].append(args.stage);save()
    except Exception as error:
        results.append({'stage':args.stage,'name':'阶段中断','passed':False,'error':str(error)});save();raise
    finally:
        stop()
        subprocess.run([str(binary),'--poll','off'],env=env,capture_output=True,check=True)
        st=state();results.append({'stage':args.stage,'name':'测试关闭且无待处理任务','passed':not st['polling'] and not any(c['status']=='pending' for c in st['claims'])});save()
        latest={r['name']:r for r in results if r['name']!='阶段中断'}
        summary={'completed':run['completed'],'messages':len(sent),'results':results,'final_results':list(latest.values()),'all_passed':args.stage in run['completed'] and all(r['passed'] for r in latest.values())}
        (out/'summary.json').write_text(json.dumps(summary,ensure_ascii=False,indent=2))
        print(json.dumps({'stage':args.stage,'completed':run['completed'],'messages':len(sent),'passed':sum(r['passed'] for r in results)},ensure_ascii=False),flush=True)
    return 0

if __name__=='__main__':raise SystemExit(main())
