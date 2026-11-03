import type { BrokerFault } from './faults.js';
/** Advice only. No command execution or recovery request is performed here. */
export function recoveryAdvice(fault: BrokerFault, operation?: string): { stage: string; nextActions: string[] } {
  if (fault.code === 'REQUEST_ABORTED') return { stage: 'cancelled', nextActions: [] };
  if (fault.code === 'CLIENT_CLOSED') return { stage: 'host', nextActions: ['open_or_restart_host'] };
  if (fault.code === 'BROKER_CONTEXT_UNAVAILABLE') return { stage: 'profile', nextActions: ['inspect_profile_then_rebind'] };
  if (fault.code === 'VALIDATION_FAILED' && operation !== 'register_agent') return { stage: 'protocol', nextActions: ['check_provider_contract'] };
  if (fault.code === 'CREDENTIAL_FILE_INVALID') return { stage: 'credentials', nextActions: ['prepare_cli_profile', 'open_or_restart_host'] };
  if (['BROKER_UNAUTHORIZED', 'LOCAL_AUTH_FORBIDDEN'].includes(fault.code)) return { stage: 'local_auth', nextActions: ['repair_private_file'] };
  if (fault.code === 'CENTRAL_UNAUTHENTICATED') return { stage: 'identity', nextActions: ['stop_then_reauthenticate_agent'] };
  if (['BROKER_FORBIDDEN', 'BROKER_CONTEXT_INVALID', 'VALIDATION_FAILED'].includes(fault.code)) return { stage: 'identity', nextActions: ['inspect_profile_then_rebind'] };
  if (['SESSION_CONFLICT', 'SESSION_REPLACED'].includes(fault.code)) return { stage: 'binding', nextActions: ['inspect_connection_owner'] };
  if (['SESSION_EXPIRED', 'AGENT_UNREGISTERED'].includes(fault.code)) return { stage: 'binding', nextActions: ['register_agent'] };
  if (fault.code === 'CENTRAL_RESULT_UNKNOWN') return { stage: 'registration', nextActions: ['inspect_owned_agents_then_use'] };
  if (fault.code.startsWith('CENTRAL_')) return { stage: 'central', nextActions: ['check_network_then_retry'] };
  if (fault.category === 'transport') return { stage: 'broker', nextActions: ['inspect_local_broker'] };
  return { stage: 'protocol', nextActions: ['check_provider_contract'] };
}
