import { render } from 'solid-js/web'
import './style.css'
import App from './App'

async function main() {
  if (import.meta.env.DEV && new URLSearchParams(location.search).has('mock')) {
    (await import('../mock/mock')).install()
  }
  render(() => <App />, document.getElementById('root')!)
}
main()
