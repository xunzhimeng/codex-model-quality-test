// 每个账号串行执行题目，账号之间有界并发；异常账号可跳过余项，不中止其他账号。
export async function runAccountQueue<A, T>(accounts: A[], tasks: T[], concurrency: number, execute: (account: A, task: T) => Promise<void | boolean>, stopped: () => boolean) {
  let next = 0
  async function worker() {
    while (!stopped()) {
      const index = next++
      if (index >= accounts.length) return
      for (const task of tasks) {
        if (stopped()) return
        if (await execute(accounts[index], task) === false) break
      }
    }
  }
  await Promise.all(Array.from({ length: Math.min(concurrency, accounts.length) }, worker))
}
export function csvCell(value: unknown) {
  let text = String(value ?? '')
  if (/^[\s]*[=+\-@]/.test(text) || /^[\t\r\n]/.test(text)) text = `'${text}`
  return `"${text.replaceAll('"', '""')}"`
}
