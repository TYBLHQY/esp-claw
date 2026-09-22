import { createSignal, type Component } from 'solid-js';
import { downloadConfigBackup, restoreConfigBackup, startOta } from '../api/client';
import { t } from '../i18n';
import { Button } from '../components/ui/Button';
import { TextInput } from '../components/ui/FormField';
import { PageHeader } from '../components/ui/PageHeader';
import { TabShell } from '../components/layout/TabShell';
import { pushToast } from '../state/toast';

export const MaintenancePage: Component = () => {
  const [otaUrl, setOtaUrl] = createSignal('');
  const [busy, setBusy] = createSignal(false);
  let fileInput: HTMLInputElement | undefined;

  const backup = async () => {
    try { await downloadConfigBackup(); pushToast(t('maintenanceBackupDone') as string, 'success'); }
    catch (err) { pushToast((err as Error).message, 'error'); }
  };
  const restore = async () => {
    const file = fileInput?.files?.[0];
    if (!file) return;
    setBusy(true);
    try { const result = await restoreConfigBackup(file); pushToast(result.message || (t('maintenanceRestoreDone') as string), 'success'); }
    catch (err) { pushToast((err as Error).message, 'error'); }
    finally { setBusy(false); }
  };
  const ota = async () => {
    setBusy(true);
    try { const result = await startOta(otaUrl().trim()); pushToast(result.message || (t('maintenanceOtaStarted') as string), 'success'); }
    catch (err) { pushToast((err as Error).message, 'error'); }
    finally { setBusy(false); }
  };

  return <TabShell>
    <PageHeader title={t('navMaintenance') as string} description={t('maintenanceDesc') as string} />
    <div class="space-y-5 p-5">
      <section class="space-y-3">
        <h2 class="text-sm font-semibold">{t('maintenanceConfig') as string}</h2>
        <div class="flex flex-wrap gap-2">
          <Button size="sm" variant="secondary" onClick={backup}>{t('maintenanceBackup') as string}</Button>
          <input ref={fileInput} type="file" accept="application/json" />
          <Button size="sm" variant="secondary" disabled={busy()} onClick={restore}>{t('maintenanceRestore') as string}</Button>
        </div>
      </section>
      <section class="space-y-3">
        <h2 class="text-sm font-semibold">{t('maintenanceOta') as string}</h2>
        <TextInput label={t('maintenanceOtaUrl')} value={otaUrl()} onInput={(e) => setOtaUrl(e.currentTarget.value)} placeholder="https://example.com/esp-claw.bin" />
        <Button size="sm" variant="primary" disabled={busy() || !otaUrl().startsWith('https://')} onClick={ota}>{t('maintenanceOtaStart') as string}</Button>
      </section>
    </div>
  </TabShell>;
};
