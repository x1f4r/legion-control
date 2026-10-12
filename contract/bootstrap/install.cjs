const fs=require('node:fs'),path=require('node:path'),crypto=require('node:crypto'),cp=require('node:child_process');
const [base,archive,hash,size,op]=process.argv.slice(1), incoming=path.join(base,'incoming');
const stat=fs.statSync(archive); if(stat.size!==Number(size)) throw Error('Uploaded archive size does not match');
const bytes=fs.readFileSync(archive); if(crypto.createHash('sha256').update(bytes).digest('hex')!==hash) throw Error('Uploaded archive hash does not match');
if(fs.existsSync(path.join(incoming,'agent'))) throw Error('An incoming agent tree already exists; nothing was replaced');
const unpack=cp.spawnSync('tar',['-xzf',archive,'-C',incoming],{encoding:'utf8',timeout:30000});
if(unpack.status!==0) throw Error(unpack.stderr||'Archive extraction failed');
const installed=cp.spawnSync(process.execPath,[path.join(incoming,'agent','src','index.mjs'),'self-update','--install','--op',op],{env:{...process.env,LEGIONCTL_HOME:base},stdio:'inherit',timeout:240000});
process.exit(installed.status===null?1:installed.status);