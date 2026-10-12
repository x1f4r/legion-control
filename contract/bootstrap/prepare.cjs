const fs=require('node:fs'),path=require('node:path');
if(Number(process.versions.node.split('.')[0])<24) throw Error('Node 24 or newer is required before this upgrade');
const base=process.argv[1], incoming=path.join(base,'incoming');
if(!fs.existsSync(path.join(base,'agent','src','index.mjs'))) throw Error('No installed agent exists in this base; bootstrap refused');
fs.mkdirSync(incoming,{recursive:true});
if(fs.existsSync(path.join(incoming,'agent'))) throw Error('An incoming agent tree already exists. Inspect and recover it before trying another installation');
console.log(JSON.stringify({ok:true}));