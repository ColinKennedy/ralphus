p='daemon/src/reviews.rs'
s=open(p,encoding='utf-8').read()
s=s.replace("prefetched_upstreams: &HashMap<(String, String), String>,","prefetched_upstreams: &HashMap<(String, UpstreamRef), UpstreamRef>,")
s=s.replace('.map_or_else(|| "main".to_string(), |m| m.upstream.clone());','.map_or_else(|| UpstreamRef::local("main"), |m| m.upstream.clone());')
open(p,'w',encoding='utf-8').write(s)
