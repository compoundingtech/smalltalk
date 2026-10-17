import * as stylex from '@stylexjs/stylex'

const styles = stylex.create({ label: { color: 'rgb(10, 80, 170)', fontWeight: 500 } })
const label = document.createElement('p')
label.dataset.styleProof = ''
label.textContent = 'Fictional stylesheet recovery proof'
label.className = stylex.props(styles.label).className ?? ''
document.body.append(label)
