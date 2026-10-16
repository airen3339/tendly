<script setup lang="ts">
import { computed, defineAsyncComponent, onMounted, reactive, ref, watch } from 'vue';
import type { CompletionSource } from '@codemirror/autocomplete';
import { request, json, errorMessage } from './api';
import { BALANCE_EXAMPLE, BALANCE_EXAMPLES, authFields, authFromFields, type BalanceConfig, type BalanceReading, type BalanceView } from './balance-model';
import { applyView } from './balance-store';
import BalanceResult from './BalanceResult.vue';
import Icon from './Icon.vue';
import { useI18n } from '../i18n';
// CodeMirror loads with the first balance editor, not with the app.
const CodeEditor=defineAsyncComponent(()=>import('./CodeEditor.vue'));
const props=defineProps<{profileId:string; disabled?:boolean}>();
const {t}=useI18n();
const mode=ref<'disabled'|'money'|'percentage'>('disabled'), currency=ref('USD'), queryUrl=ref(''), secret=ref(''), script=ref(BALANCE_EXAMPLE);
const loaded=ref(false), busy=ref(false), error=ref(''), message=ref(''), view=ref<BalanceView>(), preview=ref<BalanceReading>();
const baseline=ref('');
const auth=reactive(authFields({kind:'header',name:'Authorization',value:'Bearer {key}'}));
const url=`/endpoint-profiles/${encodeURIComponent(props.profileId)}/balance`;
const MODES=[{id:'disabled',label:'Disabled'},{id:'money',label:'Fiat balance'},{id:'percentage',label:'Remaining percentage'}] as const;
function config():BalanceConfig|null {
  if(mode.value==='disabled')return null;
  return {unit:mode.value==='money'?{kind:'money',currency:currency.value.trim().toUpperCase()}:{kind:'percentage'},query_url:queryUrl.value.trim(),auth:authFromFields(auth),secret_ref:secret.value.trim()||null,script:script.value};
}
watch(() => JSON.stringify(config()), () => { preview.value=undefined; message.value=''; }, {flush:'sync'});
const dirty=computed(()=>loaded.value&&JSON.stringify(config())!==baseline.value);
const disabled=computed(()=>busy.value||props.disabled||!loaded.value);
function apply(next:BalanceView){
  view.value=next;const c=next.config;
  mode.value=c?.unit.kind??'disabled';currency.value=c?.unit.kind==='money'?c.unit.currency:'USD';queryUrl.value=c?.query_url??'';Object.assign(auth,authFields(c?.auth??{kind:'header',name:'Authorization',value:'Bearer {key}'}));secret.value=c?.secret_ref??'';script.value=c?.script??BALANCE_EXAMPLE;
  baseline.value=JSON.stringify(config());loaded.value=true;preview.value=undefined;
}
async function load(){error.value='';try{apply(await request<BalanceView>(url));}catch(cause){error.value=errorMessage(cause);}}
onMounted(load);
function reset(){if(view.value)apply(view.value);}
defineExpose({dirty,busy,reset});
async function save(){busy.value=true;error.value='';message.value='';try{const next=await request<BalanceView>(url,json('PUT',config()));apply(next);applyView(props.profileId,next);message.value=t('Balance query saved');}catch(cause){error.value=errorMessage(cause);}finally{busy.value=false;}}
async function test(){busy.value=true;error.value='';message.value='';preview.value=undefined;try{preview.value=await request<BalanceReading>(url+'/test',json('POST',config()));}catch(cause){error.value=errorMessage(cause);}finally{busy.value=false;}}
async function refresh(){busy.value=true;error.value='';message.value='';try{view.value=await request<BalanceView>(url+'/query',json('POST'));applyView(props.profileId,view.value);preview.value=undefined;}catch(cause){error.value=errorMessage(cause);}finally{busy.value=false;}}
/** The help sentence with its code set apart; the code reads the same in every language. */
const codeParts=(text:string)=>text.split(/(await ctx\.request\(\)|\{ value \})/);
function useExample(event:Event){const select=event.target as HTMLSelectElement;const example=BALANCE_EXAMPLES.find(entry=>entry.id===select.value);if(example)script.value=example.script;select.value='';}

/** What a script can reach: ctx.request(), and the words its body is built from. */
const completions:CompletionSource=context=>{
  const member=context.matchBefore(/ctx\.\w*/);
  if(member)return {from:member.from+4,options:[{label:'request',type:'method',detail:'(options?) → JSON',apply:'request()',info:t('GET the query URL, or pass { url, method, headers, body }. Returns parsed JSON.')}]};
  const word=context.matchBefore(/\w+/);
  if(!word||(word.from===word.to&&!context.explicit))return null;
  return {from:word.from,options:[
    {label:'ctx',type:'variable',detail:'Tendly'},
    {label:'await ctx.request()',type:'function',detail:'→ JSON'},
    {label:'return { value }',type:'keyword',apply:'return { value:  };',detail:t('the balance')},
  ]};
};
</script>
<template>
  <section class="balance-editor" :aria-label="t('Custom balance query')">
    <header class="balance-editor-head">
      <h4><Icon name="wallet" :size="15" />{{ t('Custom balance query') }}</h4>
      <span v-if="dirty" class="balance-dirty">{{ t('Unsaved changes') }}</span>
    </header>
    <p v-if="error" class="inline-error" role="alert">{{ t(error) }} <button v-if="!loaded" type="button" class="text-button" @click="load">{{ t('Retry') }}</button></p>
    <fieldset class="form-stack" :disabled="disabled">
      <div class="balance-modes" role="radiogroup" :aria-label="t('Balance type')">
        <button v-for="entry in MODES" :key="entry.id" type="button" role="radio" :aria-checked="mode===entry.id" :class="{selected:mode===entry.id}" @click="mode=entry.id">{{ t(entry.label) }}</button>
      </div>
      <template v-if="mode!=='disabled'">
        <div class="balance-grid">
          <label class="wide">{{ t('Balance query URL') }}<input v-model="queryUrl" type="url" placeholder="https://api.example.com/balance" autocomplete="off" spellcheck="false" /></label>
          <label v-if="mode==='money'">{{ t('Currency code') }}<input v-model="currency" maxlength="3" placeholder="USD / CNY" spellcheck="false" autocomplete="off" class="field-mono balance-currency" /></label>
          <label>{{ t('Authentication') }}<select v-model="auth.preset"><option value="bearer">{{ t('Bearer token (Authorization header)') }}</option><option value="api_key">{{ t('x-api-key header') }}</option><option value="header">{{ t('Custom header') }}</option><option value="query">{{ t('Query parameter') }}</option><option value="none">{{ t('No authentication') }}</option></select></label>
          <template v-if="auth.preset==='header'"><label>{{ t('Header name') }}<input v-model="auth.headerName" placeholder="X-Token" autocomplete="off" spellcheck="false" class="field-mono" /></label><label>{{ t('Header value') }}<input v-model="auth.headerValue" placeholder="Bearer {key}" autocomplete="off" spellcheck="false" class="field-mono" /><small>{{ t('{key} is replaced by the key on the server.') }}</small></label></template>
          <label v-else-if="auth.preset==='query'">{{ t('Parameter name') }}<input v-model="auth.queryName" placeholder="api_key" autocomplete="off" spellcheck="false" class="field-mono" /></label>
          <label v-if="auth.preset!=='none'" class="wide">{{ t('Balance key reference') }}<input v-model="secret" class="field-mono" :placeholder="t('Empty uses the endpoint key · env:TENDLY_SECRET_…')" autocomplete="off" spellcheck="false" /></label>
        </div>
        <div class="balance-script">
          <div class="balance-script-head">
            <span>{{ t('Balance JavaScript') }}</span>
            <select class="balance-examples" :aria-label="t('Insert an example')" @change="useExample"><option value="">{{ t('Insert an example…') }}</option><option v-for="entry in BALANCE_EXAMPLES" :key="entry.id" :value="entry.id">{{ t(entry.label) }}</option></select>
          </div>
          <CodeEditor v-model="script" :label="t('Balance JavaScript')" :disabled="disabled" :completions="completions" />
          <p class="form-help"><template v-for="(part,index) in codeParts(t(mode==='money'?'An async function body: read JSON with await ctx.request(), then return { value }. A decimal string keeps money exact.':'An async function body: read JSON with await ctx.request(), then return { value } as the percentage left, 0–100.'))" :key="index"><code v-if="index%2">{{ part }}</code><template v-else>{{ part }}</template></template></p>
          <details class="balance-api-help"><summary>{{ t('Request options') }}</summary><pre><code>await ctx.request({
  url: '/balance',            // {{ t('same origin as the query URL') }}
  method: 'GET',              // GET | POST
  headers: { Accept: 'application/json' },
  body: { account: 'me' },    // {{ t('sent as JSON') }}
})</code></pre><p class="form-help">{{ t('At most 4 requests, 10 seconds and 1 MiB per response.') }}</p></details>
        </div>
      </template>
    </fieldset>
    <footer class="balance-editor-foot">
      <div class="balance-outcome">
        <div v-if="preview" class="balance-preview"><strong>{{ t('Test result') }}</strong><BalanceResult :reading="preview" /></div>
        <BalanceResult v-else-if="view?.config&&!dirty" :reading="view.reading" :stale="view.stale" :error="view.error" />
        <p v-if="message" class="form-help" role="status">{{ message }}</p>
      </div>
      <div class="toolbar-buttons">
        <button v-if="view?.config" type="button" class="small-button" :disabled="disabled||dirty" @click="refresh"><Icon name="refresh" :size="13" />{{ t('Refresh saved balance') }}</button>
        <button v-if="mode!=='disabled'" type="button" class="small-button" :disabled="disabled" @click="test"><Icon name="play" :size="13" />{{ t(busy?'Working…':'Test balance script') }}</button>
        <button type="button" class="small-button primary" :disabled="disabled||!dirty" @click="save">{{ t('Save balance query') }}</button>
      </div>
    </footer>
  </section>
</template>
<style scoped>
.balance-editor{display:grid;gap:14px;margin:16px 0;padding:16px;border:1px solid var(--line);border-radius:var(--radius-lg);background:var(--surface)}
.balance-editor-head{display:flex;align-items:center;gap:12px}
.balance-editor-head h4{display:flex;align-items:center;gap:7px;margin:0;font-size:var(--text-md);font-weight:600;color:var(--ink)}
.balance-editor-head h4 .app-icon{--icon-color:var(--accent)}
.balance-dirty{margin-left:auto;flex-shrink:0;padding:2px 9px;border-radius:var(--radius-full);background:var(--warn-soft);color:var(--warn-ink);font-size:var(--text-xs);white-space:nowrap}
.balance-editor fieldset{display:grid;gap:14px;min-width:0;margin:0;padding:0;border:0}
.balance-modes{display:inline-flex;justify-self:start;padding:3px;border-radius:var(--radius-full);background:var(--fill)}
.balance-modes button{height:28px;padding:0 14px;border:0;border-radius:var(--radius-full);background:none;color:var(--ink-soft);font-size:var(--text-sm);cursor:pointer;white-space:nowrap}
.balance-modes button.selected{background:var(--surface);color:var(--ink);font-weight:600;box-shadow:var(--shadow-sm)}
.balance-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,180px),1fr));gap:12px}
.balance-grid .wide{grid-column:1 / -1}
.balance-editor label{display:grid;gap:6px;font-size:var(--text-sm);color:var(--ink-soft);min-width:0}
.balance-editor input,.balance-editor select{width:100%;box-sizing:border-box}
.balance-currency{text-transform:uppercase}
.balance-editor small{color:var(--muted);font-size:var(--text-xs);font-weight:400}
.balance-script{display:grid;gap:8px}
.balance-script-head{display:flex;align-items:center;gap:10px;font-size:var(--text-sm);font-weight:550;color:var(--ink-soft)}
.balance-examples{margin-left:auto;width:auto !important;max-width:55%;min-height:28px;padding:3px 10px;font-size:var(--text-xs)}
/* A balance script is a few lines; the shared editor's height is sized for longer code. */
.balance-script .code-editor :deep(.cm-content),.balance-script .code-editor :deep(.cm-gutter){min-height:96px}
.balance-script .form-help code{padding:0 4px;border-radius:var(--radius-xs);background:var(--fill);color:var(--ink-soft);font-size:.9em}
.balance-api-help{font-size:var(--text-xs);color:var(--muted)}
.balance-api-help summary{cursor:pointer;width:max-content}
.balance-api-help pre{margin:8px 0;padding:10px 12px;border-radius:var(--radius-md);background:var(--code-line);overflow:auto;font:var(--text-xs)/1.6 var(--mono);color:var(--syn-text)}
.balance-editor-foot{display:flex;align-items:flex-end;justify-content:space-between;flex-wrap:wrap;gap:12px;padding-top:12px;border-top:1px solid var(--line)}
.balance-outcome{display:grid;gap:8px;min-width:0;flex:1 1 220px}
.balance-preview{padding:10px 12px;border:1px dashed var(--accent-line);border-radius:var(--radius-md);background:var(--accent-soft)}
.balance-preview>strong{display:block;font-size:var(--text-xs);color:var(--accent-ink);margin-bottom:6px}
/* The shared toolbar style never shrinks; here the row wraps inside the card instead of running past it. */
.toolbar-buttons{display:flex;flex-wrap:wrap;justify-content:flex-end;gap:8px;margin-left:auto;flex-shrink:1;min-width:0}
.toolbar-buttons .small-button{display:inline-flex;align-items:center;gap:5px}
.form-help{margin:0}
</style>
