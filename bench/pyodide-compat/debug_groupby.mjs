import { loadPyodide } from 'pyodide';
const py = await loadPyodide();
await py.loadPackage(['numpy', 'pandas']);
try {
  await py.runPythonAsync(`
import pandas as pd
df = pd.DataFrame({'g':['a','b','a','b','a'],'v':[1,2,3,4,5]})
g = df.groupby('g').agg({'v':['sum','mean','count']})
print('groupby ok:')
print(g)
print('to_dict:')
print(g.to_dict())
`);
} catch (e) {
  console.error('FAIL:', e.message);
}
