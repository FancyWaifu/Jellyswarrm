//! Server switcher.
//!
//! Adds a "Servers" section to the web client's nav drawer listing every
//! federated backend. Picking one stores its id in `localStorage` and the
//! injected script attaches an `X-Js-Server` header to same-origin API calls;
//! the federated read handlers (see `federated::scope_server_id`) then show only
//! that backend's libraries and content. "All servers" clears the scope and
//! returns to the merged federation.

use axum::{extract::State, response::IntoResponse, Json};
use hyper::header;
use serde::Serialize;
use tracing::error;

use crate::AppState;

#[derive(Serialize)]
struct ServerInfo {
    id: i64,
    name: String,
}

/// `GET /servers` — the federated backends, for the switcher UI.
pub async fn list_servers(State(state): State<AppState>) -> impl IntoResponse {
    match state.server_storage.list_servers().await {
        Ok(servers) => Json(
            servers
                .into_iter()
                .map(|s| ServerInfo {
                    id: s.id,
                    name: s.name,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => {
            error!("Failed to list servers for switcher: {}", e);
            Json(Vec::<ServerInfo>::new()).into_response()
        }
    }
}

/// Injected into the served web client. Patches fetch/XHR to carry the scope
/// header, and renders the server list into the nav drawer.
const INJECT_JS: &str = r#"(function(){
  var KEY='js_server';
  function scope(){try{return localStorage.getItem(KEY)||'';}catch(e){return '';}}
  function same(u){try{return new URL(u, location.href).origin===location.origin;}catch(e){return true;}}
  // --- attach X-Js-Server to same-origin API calls when a scope is set ---
  var of=window.fetch;
  if(of){
    window.fetch=function(input,init){
      var s=scope();
      if(s){
        var url=(typeof input==='string')?input:(input&&input.url)||'';
        if(same(url)){
          init=init||{};
          var h=new Headers((init&&init.headers)||(typeof input==='object'&&input&&input.headers)||{});
          h.set('X-Js-Server',s); init.headers=h;
        }
      }
      return of.call(this,input,init);
    };
  }
  var oopen=XMLHttpRequest.prototype.open;
  XMLHttpRequest.prototype.open=function(m,u){this.__jsu=u; return oopen.apply(this,arguments);};
  var osend=XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.send=function(){
    var s=scope();
    if(s && same(this.__jsu||'')){try{this.setRequestHeader('X-Js-Server',s);}catch(e){}}
    return osend.apply(this,arguments);
  };
  // --- nav drawer section ---
  var servers=[];
  function pick(val){try{if(val)localStorage.setItem(KEY,val);else localStorage.removeItem(KEY);}catch(e){}location.assign('/');}
  function build(){
    var cont=document.querySelector('.mainDrawer-scrollContainer')||document.querySelector('.mainDrawer');
    if(!cont||!servers.length)return;
    if(document.getElementById('js-server-nav'))return;
    var cur=scope();
    var wrap=document.createElement('div');wrap.id='js-server-nav';
    wrap.style.cssText='padding:.3em 0;border-bottom:1px solid rgba(255,255,255,.12);margin-bottom:.3em';
    var hd=document.createElement('h3');hd.textContent='Servers';
    hd.style.cssText='margin:.5em 1em .2em;font-size:.8em;opacity:.6;text-transform:uppercase;letter-spacing:.05em';
    wrap.appendChild(hd);
    function row(label,val,active){
      var a=document.createElement('a');a.href='#';a.className='navMenuOption emby-button';
      a.style.cssText='display:flex;align-items:center;padding:.55em 1.1em;text-decoration:none;color:inherit;'+(active?'background:rgba(0,164,220,.22);font-weight:600;':'');
      a.textContent=(active?'▸ ':'  ')+label;
      a.onclick=function(e){e.preventDefault();pick(val);};
      wrap.appendChild(a);
    }
    row('All servers','',!cur);
    servers.forEach(function(s){row(s.name,String(s.id),cur===String(s.id));});
    cont.insertBefore(wrap,cont.firstChild);
  }
  fetch('/servers').then(function(r){return r.ok?r.json():[];}).then(function(list){
    servers=list||[];if(!servers.length)return;
    build();
    new MutationObserver(function(){
      if(!document.getElementById('js-server-nav'))build();
    }).observe(document.body,{childList:true,subtree:true});
  }).catch(function(){});
})();"#;

/// `GET /servers/inject.js` — the switcher client script.
pub async fn servers_inject_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        INJECT_JS,
    )
}
