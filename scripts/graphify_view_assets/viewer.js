/* Repository-owned presentation adapter. Graphify remains the graph authority. */
(function () {
  'use strict';
  const $ = id => document.getElementById(id);
  const PAGE = 60, RELATION_PAGE = 40, DRAW_NODES = 600, DRAW_EDGES = 100;
  const COLORS = ['#7cdae2', '#ddb879', '#9fa5e6', '#91c6ad', '#d6a1b4', '#8eafdc'];
  const reduced = window.matchMedia('(prefers-reduced-motion: reduce)');
  const graph = window.GRAPHIFY_VIEW_DATA;
  const initialHash = location.hash;
  if (!graph || !Array.isArray(graph.nodes) || !Array.isArray(graph.links)) {
    $('fatal').hidden = false;
    $('fatal').textContent = '完整图谱数据未载入。请通过项目图谱导出入口重新生成视图，并保留旁边的 view-data.js 与 view-assets 目录。';
    return;
  }
  const nodes = graph.nodes, edges = graph.links, meta = graph.meta || {};
  const directed = Boolean(graph.graph_metadata && graph.graph_metadata.directed === true);
  $('direction-note').textContent = '实线为直接提取，虚线为推断。' + (directed ? '箭头标记图谱关系方向。' : '关系图未编码方向。');
  const byId = new Map(), adjacency = new Map(), terms = new Map();
  let groups = [], groupMap = new Map(), grouping = 'module', scope = '', query = '';
  let resultPage = 0, relationPage = 0, selected = null, lastFocus = null;
  let filtered = [], searching = false, timer = null, searchStart = 0;
  const metrics = {nodeCount: nodes.length, edgeCount: edges.length, filters: [], frames: 0, mode: 'pending'};
  window.HERDR_ATLAS_METRICS = metrics;
  const fmt = value => Number(value).toLocaleString('en-US');
  function el(tag, text, cls) { const n = document.createElement(tag); if (text !== undefined) n.textContent = text; if (cls) n.className = cls; return n; }
  function hash(text) { let h = 2166136261; for (let i = 0; i < String(text).length; i++) h = Math.imul(h ^ String(text).charCodeAt(i), 16777619); return h >>> 0; }
  function random(seed) { let s = seed >>> 0; return () => { s = Math.imul(s, 1664525) + 1013904223 >>> 0; return s / 4294967296; }; }
  function color(key) { return COLORS[hash(key) % COLORS.length]; }
  function groupKey(n) { return grouping === 'community' ? String(n.community ?? 'unknown') : String(n.module || '未定位引用'); }
  function groupLabel(n) { return grouping === 'community' ? (n.community_name || 'Community ' + (n.community ?? 'unknown')) : (n.module || '未定位引用'); }
  function path(n) { return n.source_path || n.source_file || ''; }
  function sourceHref(raw) {
    // Only the exporter-owned relative snapshot directory is linkable from data.
    return typeof raw === 'string' && /^view-sources\/[a-f0-9]+\.html(?:#L[1-9][0-9]*)?$/.test(raw) ? raw : null;
  }
  for (const n of nodes) {
    byId.set(n.id, n);
    terms.set(n.id, [n.label, n.id, path(n), n.module, n.community_name].join(' ').toLocaleLowerCase());
  }
  for (let i = 0; i < edges.length; i++) {
    const e = edges[i];
    if (!adjacency.has(e.source)) adjacency.set(e.source, []);
    adjacency.get(e.source).push(i);
    if (e.target !== e.source) {
      if (!adjacency.has(e.target)) adjacency.set(e.target, []);
      adjacency.get(e.target).push(i);
    }
  }
  function buildGroups() {
    groupMap = new Map();
    for (const n of nodes) {
      const key = groupKey(n);
      if (!groupMap.has(key)) groupMap.set(key, {key, label: groupLabel(n), nodes: [], color: color(key)});
      groupMap.get(key).nodes.push(n);
    }
    groups = Array.from(groupMap.values()).sort((a, b) => b.nodes.length - a.nodes.length || a.key.localeCompare(b.key));
    $('scope').replaceChildren(el('option', grouping === 'module' ? '全部模块' : '全部社区'));
    $('scope').firstChild.value = '';
    for (const g of groups) { const o = el('option', g.label + ' (' + fmt(g.nodes.length) + ')'); o.value = g.key; $('scope').append(o); }
  }
  function resultButton(n) {
    const b = el('button', undefined, 'result-item' + (n.id === selected ? ' active' : ''));
    b.dataset.nodeId = n.id;
    const dot = el('i', undefined, 'result-dot'); dot.style.color = color(groupKey(n)); dot.style.background = dot.style.color;
    const body = el('span', undefined, 'result-body');
    body.append(el('span', n.label || n.id, 'result-name'), el('span', path(n) ? path(n) + (n.line ? ':' + n.line : '') : '无源码定位 · ' + n.id, 'result-sub'));
    b.append(dot, body, el('span', '›', 'result-arrow'));
    b.addEventListener('click', () => selectNode(n.id));
    return b;
  }
  function renderResults() {
    const container = $('results'); container.replaceChildren();
    searching = Boolean(query.trim() || scope);
    if (!searching) {
      filtered = groups;
      $('list-title').textContent = grouping === 'module' ? '模块星群' : '图谱社区';
      $('result-count').textContent = fmt(groups.length) + ' 个星群';
      for (const g of groups.slice(resultPage * PAGE, (resultPage + 1) * PAGE)) {
        const b = el('button', undefined, 'result-item'); b.dataset.groupKey = g.key;
        const dot = el('i', undefined, 'result-dot'); dot.style.color = g.color; dot.style.background = g.color;
        const body = el('span', undefined, 'result-body'); body.append(el('span', g.label, 'result-name'));
        b.append(dot, body, el('span', fmt(g.nodes.length), 'result-count'), el('span', '›', 'result-arrow'));
        b.addEventListener('click', () => enterGroup(g.key)); container.append(b);
      }
    } else {
      const words = query.trim().toLocaleLowerCase().split(/\s+/).filter(Boolean);
      const pool = scope && groupMap.has(scope) ? groupMap.get(scope).nodes : nodes;
      filtered = words.length ? pool.filter(n => words.every(word => terms.get(n.id).includes(word))) : pool;
      $('list-title').textContent = query ? '搜索结果' : '星群中的节点';
      $('result-count').textContent = fmt(filtered.length) + ' 个节点';
      for (const n of filtered.slice(resultPage * PAGE, (resultPage + 1) * PAGE)) container.append(resultButton(n));
      if (!filtered.length) { const empty = el('div', undefined, 'empty-state'); empty.append(el('strong', '没有匹配的节点'), el('span', '尝试更短的符号或路径，或将范围改为全部。')); container.append(empty); }
    }
    const pages = Math.max(1, Math.ceil(filtered.length / PAGE));
    $('page-count').textContent = (resultPage + 1) + ' / ' + pages;
    $('previous').disabled = resultPage === 0; $('next').disabled = resultPage + 1 >= pages;
    container.scrollTop = 0;
  }
  function applySearch() {
    query = $('search').value; resultPage = 0; renderResults();
    metrics.lastFilterMs = performance.now() - searchStart;
    metrics.filters.push(metrics.lastFilterMs);
    if (metrics.filters.length > 30) metrics.filters.shift();
    if (scope && !selected) scene.showGroup(groupMap.get(scope), filtered);
    document.dispatchEvent(new CustomEvent('atlas-filtered', {detail: {count: filtered.length}}));
  }
  function enterGroup(key) {
    closeDetail(false); scope = key; query = ''; $('search').value = ''; $('scope').value = key; resultPage = 0;
    renderResults(); scene.showGroup(groupMap.get(key), groupMap.get(key).nodes);
    $('breadcrumb').textContent = 'herdr / ' + groupMap.get(key).label;
    $('scene-title').textContent = groupMap.get(key).label;
    $('scene-description').textContent = fmt(groupMap.get(key).nodes.length) + ' 个节点。选择一个符号，沿着关系继续探索。';
  }
  function home() {
    closeDetail(false); scope = ''; query = ''; $('search').value = ''; $('scope').value = ''; resultPage = 0;
    history.replaceState(null, '', location.pathname + location.search); renderResults(); scene.showOverview();
    $('breadcrumb').textContent = 'herdr / src'; $('scene-title').replaceChildren(document.createTextNode('在代码之间，'), el('br'), document.createTextNode('看见连接。'));
    $('scene-description').textContent = '从模块星群出发，探索真实的源码关系。';
  }
  function selectNode(id, focus = true) {
    const n = byId.get(id); if (!n) return;
    lastFocus = document.activeElement; selected = id; relationPage = 0;
    $('detail').hidden = false; document.body.classList.add('detail-open');
    $('detail-community').textContent = n.community_name || '未命名社区'; $('detail-title').textContent = n.label || n.id;
    $('detail-id').textContent = n.id; $('detail-path').textContent = path(n) + (n.line ? ':' + n.line : '');
    const href = sourceHref(n.source_url); $('source-link').hidden = !href; $('no-source').hidden = Boolean(href);
    if (href) $('source-link').href = href; else $('source-link').removeAttribute('href');
    $('breadcrumb').textContent = 'herdr / ' + groupLabel(n); $('scene-title').textContent = n.label || n.id;
    $('scene-description').textContent = '查看直接关系与来源证据。';
    renderRelations(); scene.showNode(n); history.replaceState(null, '', '#node=' + encodeURIComponent(id));
    document.querySelector('#detail .detail-content').scrollTop = 0;
    renderResults(); if (focus) $('detail-title').focus({preventScroll:true});
  }
  function closeDetail(restore = true) {
    selected = null; $('detail').hidden = true; document.body.classList.remove('detail-open');
    if (restore) {
      if (scope && groupMap.has(scope)) enterGroup(scope); else home();
      if (lastFocus && document.contains(lastFocus)) lastFocus.focus({preventScroll:true});
      else $('search').focus({preventScroll:true});
    }
  }
  function renderRelations() {
    const indexes = adjacency.get(selected) || []; const container = $('relations'); container.replaceChildren();
    $('relation-count').textContent = fmt(indexes.length);
    for (const i of indexes.slice(relationPage * RELATION_PAGE, (relationPage + 1) * RELATION_PAGE)) {
      const edge = edges[i], outgoing = edge.source === selected, other = outgoing ? edge.target : edge.source;
      const target = byId.get(other), item = el('div', undefined, 'relation');
      const b = el('button', (directed ? (outgoing ? '→ ' : '← ') : '关联 · ') + (target ? target.label : other));
      b.addEventListener('click', () => selectNode(other)); if (!target) b.disabled = true;
      const inferred = edge.confidence === 'INFERRED'; const note = el('small', (edge.relation || 'relation') + ' · ' + (inferred ? '推断' : (edge.confidence === 'EXTRACTED' ? '直接提取' : edge.confidence || '证据未注明')), inferred ? 'inferred-tag' : '');
      const href = sourceHref(edge.source_url); if (href) { const a = el('a', '来源 ↗'); a.href = href; a.target = '_blank'; a.rel = 'noopener'; note.append(a); }
      item.append(b, note); container.append(item);
    }
    if (!indexes.length) container.append(el('p', '图谱中未记录此节点的直接关系。', 'empty-state'));
    $('relation-page').textContent = (relationPage + 1) + ' / ' + Math.max(1, Math.ceil(indexes.length / RELATION_PAGE));
    $('relation-previous').disabled = relationPage === 0; $('relation-next').disabled = (relationPage + 1) * RELATION_PAGE >= indexes.length;
  }

  // A bounded scene is a presentation of the complete searchable index, never a data filter.
  function createScene() {
    const host = $('scene'), labels = $('scene-labels');
    let renderer = null, world = null, camera = null, controls = null, layer = null;
    let canvas = null, ctx = null, mode = '', width = 1, height = 1, frame = 0, dirty = true;
    let items = [], lines = [], clouds = [], zoom2d = 1, yaw2d = 0, drag = null;
    let labelEntries = [], pointPositions = [], sceneKind = 'overview', requested2d = new URLSearchParams(location.search).has('flat');
    let fallbackReason = '', frameTimes = [], lastFrame = 0;
    const T = window.AtlasVendor;
    function disposeLayer() {
      if (!layer) return;
      layer.traverse(obj => { if (obj.geometry) obj.geometry.dispose(); if (obj.material) { const ms = Array.isArray(obj.material) ? obj.material : [obj.material]; for (const m of ms) { if (m.map) m.map.dispose(); m.dispose(); } } });
      world.remove(layer); layer = null;
    }
    function setup(flat) {
      cancelAnimationFrame(frame); frame = 0;
      if (controls) controls.dispose(); controls = null;
      disposeLayer(); if (renderer) renderer.dispose(); renderer = null;
      host.replaceChildren(); canvas = null; ctx = null;
      if (!flat && T) {
        try {
          renderer = new T.WebGLRenderer({antialias:true,alpha:true,powerPreference:'high-performance'});
          renderer.setPixelRatio(Math.min(devicePixelRatio || 1, 1.75));
          world = new T.Scene(); world.fog = new T.FogExp2(0x081725, .00065);
          camera = new T.PerspectiveCamera(42, width / height, 1, 6000);
          camera.position.set(0, 380, 1120); camera.lookAt(0,0,0);
          canvas = renderer.domElement; host.append(canvas);
          if (T.OrbitControls) { controls = new T.OrbitControls(camera, canvas); controls.enableDamping = !reduced.matches; controls.dampingFactor = .08; controls.minDistance = 170; controls.maxDistance = 3200; controls.maxPolarAngle = Math.PI * .82; controls.addEventListener('change', invalidate); }
          canvas.addEventListener('webglcontextlost', e => { e.preventDefault(); fallbackReason = '图形上下文丢失，已切换二维'; setup(true); rebuild(); });
          mode = '3D';
          const gl = renderer.getContext(), ext = gl.getExtension('WEBGL_debug_renderer_info');
          metrics.renderer = ext ? gl.getParameter(ext.UNMASKED_RENDERER_WEBGL) : gl.getParameter(gl.RENDERER);
        } catch (error) { if (renderer) renderer.dispose(); renderer = null; fallbackReason = 'WebGL 不可用，已切换二维'; }
      }
      if (!renderer) {
        canvas = document.createElement('canvas'); host.replaceChildren(canvas); ctx = canvas.getContext('2d'); mode = '2D';
        canvas.addEventListener('wheel', e => { e.preventDefault(); zoom2d = Math.max(.4, Math.min(5,zoom2d * (e.deltaY > 0 ? .9 : 1.1))); invalidate(); }, {passive:false});
      }
      canvas.addEventListener('pointerdown', e => { drag = {x:e.clientX,y:e.clientY,moved:false}; if (mode === '2D') canvas.setPointerCapture(e.pointerId); });
      canvas.addEventListener('pointermove', e => { if (drag) { const delta = e.clientX - drag.x; if (Math.abs(delta) > 2 || Math.abs(e.clientY-drag.y)>2) drag.moved = true; if(mode === '2D'){yaw2d += delta * .003;drag.x=e.clientX;drag.y=e.clientY;invalidate();} } });
      canvas.addEventListener('pointerup', e => { if (drag && !drag.moved) pick(e); drag=null; });
      metrics.mode = mode; $('renderer-status').textContent = mode === '3D' ? 'WebGL · 3D' : 'Canvas · 2D';
      $('renderer-toggle').setAttribute('aria-pressed', String(mode === '2D'));
      $('renderer-toggle').textContent = mode === '2D' ? '尝试三维' : '二维视图';
      $('scene-help').textContent = mode === '3D' ? '拖动旋转 · 滚轮缩放 · 点击星团进入' : '拖动浏览 · 滚轮缩放 · 点击星团进入';
      resize();
    }
    function setStatus(text) { $('scene-status').textContent = text + (fallbackReason ? ' · ' + fallbackReason : ''); }
    function pointCloud(positions, colors, size, opacity) {
      if (!positions.length) return;
      const geometry = new T.BufferGeometry(); geometry.setAttribute('position',new T.Float32BufferAttribute(positions,3));
      geometry.setAttribute('color',new T.Float32BufferAttribute(colors,3));
      const material = new T.PointsMaterial({size,vertexColors:true,transparent:true,opacity,depthWrite:false,blending:T.AdditiveBlending,sizeAttenuation:true});
      layer.add(new T.Points(geometry, material));
    }
    function rebuild() {
      labels.replaceChildren(); labelEntries=[];
      const ordinary = items.filter(i => i.label).slice(0,sceneKind === 'overview' ? 26 : 15);
      for (const item of ordinary) {
        const button=el('button',undefined,'star-label'+(item.id===selected?' is-selected':''));
        button.append(document.createTextNode(item.label)); if(item.count) button.append(el('small',fmt(item.count)+' 节点'));
        button.addEventListener('click',()=>activate(item)); labels.append(button); labelEntries.push({button,item});
      }
      if (renderer) {
        disposeLayer(); layer=new T.Group(); world.add(layer);
        const positions=[], colors=[];
        for(const p of clouds){positions.push(p.x,p.y,p.z);const c=new T.Color(p.color);colors.push(c.r,c.g,c.b);}
        pointCloud(positions,colors,2.1,.55);
        const mainPositions=[],mainColors=[];
        for(const item of items){mainPositions.push(item.x,item.y,item.z);const c=new T.Color(item.color);mainColors.push(c.r,c.g,c.b);}
        pointCloud(mainPositions,mainColors,sceneKind==='overview'?13:7,.95);
        for(const inferred of [false,true]){
          const p=[];for(const line of lines.filter(l=>Boolean(l.inferred)===inferred)){p.push(line.a.x,line.a.y,line.a.z,line.b.x,line.b.y,line.b.z);}
          if(p.length){const geom=new T.BufferGeometry();geom.setAttribute('position',new T.Float32BufferAttribute(p,3));const mat=inferred?new T.LineDashedMaterial({color:0xe5bd7a,dashSize:7,gapSize:5,transparent:true,opacity:.38}):new T.LineBasicMaterial({color:0x6faabf,transparent:true,opacity:.20});const mesh=new T.LineSegments(geom,mat);if(inferred)mesh.computeLineDistances();layer.add(mesh);}
        }
        // Soft glows are decorative; their count does not represent graph nodes.
        const glowItems=sceneKind==='overview'?items.slice(0,45):items.filter(i=>i.id===selected).slice(0,1);
        for(const item of glowItems){const textureCanvas=document.createElement('canvas');textureCanvas.width=64;textureCanvas.height=64;const c=textureCanvas.getContext('2d');const g=c.createRadialGradient(32,32,0,32,32,32);g.addColorStop(0,item.color+'a0');g.addColorStop(.18,item.color+'55');g.addColorStop(1,item.color+'00');c.fillStyle=g;c.fillRect(0,0,64,64);const texture=new T.CanvasTexture(textureCanvas);const mat=new T.SpriteMaterial({map:texture,transparent:true,depthWrite:false,blending:T.AdditiveBlending,opacity:.55});const sprite=new T.Sprite(mat);sprite.position.set(item.x,item.y,item.z);const size=sceneKind==='overview'?75+Math.sqrt(item.count||1)*.4:90;sprite.scale.set(size,size,1);layer.add(sprite);}
      }
      metrics.drawnNodes=items.length;metrics.drawnEdges=lines.length;invalidate();
    }
    function overview() {
      sceneKind='overview';items=[];lines=[];clouds=[];zoom2d=1;
      const visible=groups.slice(0,80);
      visible.forEach((g,i)=>{
        const angle=i*2.399963229728653, radius=90+Math.sqrt(i/Math.max(1,visible.length-1))*400;
        const r=random(hash(g.key));const item={key:g.key,label:g.label,count:g.nodes.length,color:g.color,x:Math.cos(angle)*radius,y:(r()-.5)*90,z:Math.sin(angle)*radius*.68};items.push(item);
        for(let j=0;j<Math.min(170,45+Math.sqrt(g.nodes.length)*2);j++){const a=r()*Math.PI*2,rr=Math.pow(r(),.7)*(21+Math.log2(g.nodes.length+1)*2.8);clouds.push({x:item.x+Math.cos(a)*rr,y:item.y+(r()-.5)*rr*.65,z:item.z+Math.sin(a)*rr,color:g.color});}
      });
      const map=new Map(items.map(i=>[i.key,i]));const aggregates=new Map();
      for(const e of edges){const a=byId.get(e.source),b=byId.get(e.target);if(!a||!b)continue;const ak=groupKey(a),bk=groupKey(b);if(ak===bk||!map.has(ak)||!map.has(bk))continue;const key=JSON.stringify(ak<bk?[ak,bk]:[bk,ak]);if(!aggregates.has(key))aggregates.set(key,{a:map.get(ak),b:map.get(bk),count:0,inferred:true});const agg=aggregates.get(key);agg.count++;if(e.confidence==='EXTRACTED')agg.inferred=false;}
      lines=Array.from(aggregates.values()).sort((a,b)=>b.count-a.count).slice(0,36);
      addBackground();setStatus('全景显示 '+items.length+' / '+groups.length+' 个星群 · '+lines.length+' 条聚合连接 · 全部节点可搜索');resetCamera();rebuild();
    }
    function addBackground(){const r=random(425911);for(let i=0;i<600;i++)clouds.push({x:(r()-.5)*2200,y:(r()-.5)*1000-120,z:(r()-.5)*1700,color:i%5===0?'#52799b':'#91a6bd'});}
    function showGroup(g,pool) {
      if(!g)return;sceneKind='group';items=[];lines=[];clouds=[];
      const subset=pool.slice(resultPage*PAGE,resultPage*PAGE+DRAW_NODES);const map=new Map();
      subset.forEach((n,i)=>{const r=random(hash(n.id)),angle=i*2.399963, radius=20+Math.sqrt(i/Math.max(1,subset.length))*370;const item={id:n.id,label:i<14?n.label:null,color:g.color,x:Math.cos(angle)*radius,y:(r()-.5)*110,z:Math.sin(angle)*radius*.72};items.push(item);map.set(n.id,item);});
      for(const e of edges){if(lines.length>=DRAW_EDGES)break;if(map.has(e.source)&&map.has(e.target))lines.push({a:map.get(e.source),b:map.get(e.target),inferred:e.confidence==='INFERRED'});}
      addBackground();setStatus('局部显示 '+items.length+' / '+pool.length+' 个节点 · '+lines.length+' 条关系 · 列表分页可达全部');resetCamera();rebuild();
    }
    function showNode(n) {
      sceneKind='node';items=[];lines=[];clouds=[];
      const center={id:n.id,label:n.label,color:'#b6f4f0',x:0,y:0,z:0};items.push(center);const map=new Map([[n.id,center]]);
      const indexes=adjacency.get(n.id)||[];
      for(const idx of indexes){const e=edges[idx],other=e.source===n.id?e.target:e.source;if(!byId.has(other))continue;if(!map.has(other)&&map.size<DRAW_NODES){const target=byId.get(other),i=map.size,r=random(hash(other)),a=i*2.399963,rr=140+Math.sqrt(i)*22;const item={id:other,label:i<15?target.label:null,color:color(groupKey(target)),x:Math.cos(a)*rr,y:(r()-.5)*130,z:Math.sin(a)*rr*.7};items.push(item);map.set(other,item);}if(lines.length<DRAW_EDGES&&map.has(e.source)&&map.has(e.target))lines.push({a:map.get(e.source),b:map.get(e.target),inferred:e.confidence==='INFERRED'});}
      addBackground();setStatus('显示 '+(items.length-1)+' 个邻居 · '+lines.length+' / '+indexes.length+' 条直接关系 · 完整关系见详情');resetCamera();rebuild();
    }
    function project(item) {
      if(renderer){const v=new T.Vector3(item.x,item.y,item.z).project(camera);return{x:(v.x+1)*width/2,y:(1-v.y)*height/2,visible:v.z<1&&v.z>-1};}
      const scale=Math.min(width/1180,height/780)*zoom2d;const x=item.x*Math.cos(yaw2d)+item.z*Math.sin(yaw2d);const z=-item.x*Math.sin(yaw2d)+item.z*Math.cos(yaw2d);
      return{x:width*(width<720?.53:.57)+x*scale,y:height*(width<720?(sceneKind==='node'?.27:.36):.50)+(z*.62-item.y)*scale,visible:true};
    }
    function draw2d(){
      ctx.clearRect(0,0,width,height);
      for(const p of clouds){const v=project(p);ctx.globalAlpha=.35;ctx.fillStyle=p.color;ctx.fillRect(v.x,v.y,1.1,1.1);}
      ctx.globalAlpha=1;
      for(const line of lines){const a=project(line.a),b=project(line.b);ctx.beginPath();ctx.moveTo(a.x,a.y);ctx.lineTo(b.x,b.y);ctx.strokeStyle=line.inferred?'#d2ab6755':'#6f9fb73b';ctx.lineWidth=.8;ctx.setLineDash(line.inferred?[4,4]:[]);ctx.stroke();}ctx.setLineDash([]);
      for(const item of items){const p=project(item),size=sceneKind==='overview'?5:3;const glow=ctx.createRadialGradient(p.x,p.y,0,p.x,p.y,size*7);glow.addColorStop(0,item.color+'66');glow.addColorStop(1,item.color+'00');ctx.fillStyle=glow;ctx.fillRect(p.x-size*7,p.y-size*7,size*14,size*14);ctx.beginPath();ctx.arc(p.x,p.y,size,0,Math.PI*2);ctx.fillStyle=item.color;ctx.fill();}
    }
    function updateLabels(){
      const occupied=[];pointPositions=items.map(item=>({item,...project(item)}));
      for(const entry of labelEntries){const p=project(entry.item);entry.button.style.left=p.x+'px';entry.button.style.top=p.y+'px';const box={x:p.x,y:p.y,w:Math.min(140,entry.item.label.length*7+18),h:40};const blocked=occupied.some(b=>Math.abs(b.x-box.x)<(b.w+box.w)/2&&Math.abs(b.y-box.y)<35);const visible=p.visible&&!blocked&&p.x>0&&p.x<width&&p.y>0&&p.y<height-45;entry.button.hidden=!visible;if(visible)occupied.push(box);}
    }
    function render(now){frame=0;if(document.hidden)return;if(controls)controls.update();if(renderer)renderer.render(world,camera);else if(ctx)draw2d();updateLabels();metrics.frames++;if(!metrics.firstFrameMs)metrics.firstFrameMs=performance.now();if(lastFrame&&now-lastFrame<500){frameTimes.push(now-lastFrame);if(frameTimes.length>120)frameTimes.shift();metrics.fps=1000/(frameTimes.reduce((a,b)=>a+b,0)/frameTimes.length);}lastFrame=now;dirty=false;}
    function invalidate(){dirty=true;if(!document.hidden&&!frame)frame=requestAnimationFrame(render);}
    function frameCamera(){if(!camera)return;if(width<720)camera.setViewOffset(width,height,0,height*(sceneKind==='node'?.23:.14),width,height);else camera.clearViewOffset();camera.updateProjectionMatrix();}
    function resize(){const wasNarrow=width<720;width=host.clientWidth||1;height=host.clientHeight||1;if(renderer){renderer.setSize(width,height);camera.aspect=width/height;frameCamera();}else if(canvas){const dpr=Math.min(devicePixelRatio||1,2);canvas.width=width*dpr;canvas.height=height*dpr;canvas.style.width=width+'px';canvas.style.height=height+'px';ctx.setTransform(dpr,0,0,dpr,0,0);}if(wasNarrow!==(width<720))resetCamera();invalidate();}
    function resetCamera(){zoom2d=1;yaw2d=0;if(camera){camera.position.set(width<720?0:-130,430,width<720?(sceneKind==='node'?1900:2400):1120);frameCamera();if(controls){controls.target.set(width<720?0:-90,0,0);controls.update();}else camera.lookAt(0,0,0);}invalidate();}
    function activate(item){if(item.id)selectNode(item.id);else if(item.key!==undefined)enterGroup(item.key);}
    function pick(event){const rect=canvas.getBoundingClientRect(),x=event.clientX-rect.left,y=event.clientY-rect.top;let best=null,dist=20;for(const p of pointPositions){const d=Math.hypot(p.x-x,p.y-y);if(p.visible&&d<dist){best=p.item;dist=d;}}if(best)activate(best);}
    function zoom(factor){if(camera){camera.position.multiplyScalar(factor);if(controls)controls.update();}else zoom2d=Math.max(.4,Math.min(5,zoom2d/factor));invalidate();}
    $('zoom-in').addEventListener('click',()=>zoom(.8));$('zoom-out').addEventListener('click',()=>zoom(1.25));$('reset-camera').addEventListener('click',resetCamera);
    $('renderer-toggle').addEventListener('click',()=>{requested2d=mode==='3D';fallbackReason='';setup(requested2d);rebuild();});
    document.addEventListener('visibilitychange',()=>{metrics.hidden=document.hidden;if(document.hidden){cancelAnimationFrame(frame);frame=0;lastFrame=0;}else invalidate();});
    reduced.addEventListener('change',()=>{if(controls)controls.enableDamping=!reduced.matches;invalidate();});
    new ResizeObserver(resize).observe(host);
    setup(requested2d);
    return{showOverview:overview,showGroup,showNode,resize};
  }

  buildGroups();
  const scene=createScene();
  $('total-nodes').textContent=fmt(nodes.length)+' 节点';$('total-edges').textContent=fmt(edges.length)+' 关系';
  $('index-count').textContent=fmt(nodes.length);$('index-status').textContent='完整索引 · '+fmt(meta.source_file_count||0)+' 个源文件';
  $('fingerprint').textContent=String(meta.graph_sha256||'').slice(0,8);
  $('fingerprint').title='Graph SHA256: '+(meta.graph_sha256||'未提供');
  $('search').addEventListener('input',()=>{clearTimeout(timer);searchStart=performance.now();timer=setTimeout(applySearch,65);});
  $('scope').addEventListener('change',()=>{scope=$('scope').value;if(scope)enterGroup(scope);else home();});
  $('grouping').addEventListener('change',()=>{grouping=$('grouping').value;buildGroups();home();});
  $('previous').addEventListener('click',()=>{if(resultPage>0){resultPage--;renderResults();if(scope&&!selected)scene.showGroup(groupMap.get(scope),filtered);}});
  $('next').addEventListener('click',()=>{if((resultPage+1)*PAGE<filtered.length){resultPage++;renderResults();if(scope&&!selected)scene.showGroup(groupMap.get(scope),filtered);}});
  $('relation-previous').addEventListener('click',()=>{if(relationPage>0){relationPage--;renderRelations();}});
  $('relation-next').addEventListener('click',()=>{if((relationPage+1)*RELATION_PAGE<(adjacency.get(selected)||[]).length){relationPage++;renderRelations();}});
  $('close-detail').addEventListener('click',()=>closeDetail());$('home').addEventListener('click',home);
  document.querySelector('.brand').addEventListener('click',e=>{e.preventDefault();home();});
  $('toggle-panel').addEventListener('click',()=>{const hidden=document.body.classList.toggle('nav-hidden');$('toggle-panel').setAttribute('aria-expanded',String(!hidden));$('toggle-panel').textContent=hidden?'展开导航':'收起导航';});
  document.addEventListener('keydown',e=>{const tag=document.activeElement.tagName;if(e.key==='/'&&!['INPUT','TEXTAREA','SELECT'].includes(tag)){e.preventDefault();document.body.classList.remove('nav-hidden');$('toggle-panel').setAttribute('aria-expanded','true');$('toggle-panel').textContent='收起导航';$('search').focus();}if(e.key==='Escape'){if(selected)closeDetail();else if($('search').value){$('search').value='';searchStart=performance.now();applySearch();}}});
  home();
  if(initialHash.startsWith('#node=')){try{selectNode(decodeURIComponent(initialHash.slice(6)),false);}catch(error){/* A malformed user bookmark leaves the full index usable. */}}
  metrics.readyMs=performance.now();metrics.indexReadyMs=metrics.readyMs;
  document.body.dataset.ready='true';
  window.HERDR_ATLAS={selectNode,home,counts:()=>({nodes:nodes.length,edges:edges.length}),searchResultIds:()=>searching?filtered.map(n=>n.id):[],get selected(){return selected;}};
})();
