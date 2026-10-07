import { defineCatalog, packageJson } from '../../../../repos/effect-utils/genie/external.ts'
const catalog = defineCatalog({effect:'4.0.0-rc.118'})
const client = {data:{name:'@smalltalk/st3-client'},meta:{workspace:{repoName:'smalltalk',memberPath:'clients/typescript/st3-client',deps:[]}}}
const deps = catalog.compose({workspace:{repoName:'smalltalk',memberPath:'apps/fractal-web/packages/st3-sdk'},dependencies:{workspace:[client],external:catalog.pick('effect')}})
export default packageJson({ name:'@st3/sdk',version:'0.1.0',private:true,type:'module',exports:{'./effect':'./src/effect/mod.ts'} },deps)
