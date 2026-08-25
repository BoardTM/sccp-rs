//! bridge effects backend-effect translation.

use super::{
    AsteriskBackend, AsteriskBackendError, AsteriskChannel, BargeOperation, BridgeBackend,
    BridgeOperation, CallFeatureProvider as _, ConferenceAnnouncementOperation, MutexExt as _,
    NonNull, TransferCompletion, native_channel, play_conference_announcement, with_channel,
    with_channels, with_two_channels,
};

impl BridgeBackend for AsteriskBackend<'_> {
    fn transfer(&self, operation: &TransferCompletion) -> Result<(), Self::Error> {
        let first = operation.source.pbx_call_id;
        let second = operation.consultation.pbx_call_id;
        let result = with_two_channels(self.access, first, second, |first, second| {
            NonNull::new(first)
                .zip(NonNull::new(second))
                .ok_or(())
                .and_then(|(first, second)| {
                    match unsafe { native_channel::attended_transfer(first, second) } {
                        native_channel::AttendedTransferResult::Success => Ok(()),
                        _ => Err(()),
                    }
                })
        });
        if result == Some(Ok(())) {
            Ok(())
        } else {
            Err(AsteriskBackendError::Failed {
                operation: "bridge transfer",
                calls: format!("{} and {}", first.0, second.0),
            })
        }
    }

    fn bridge(&self, operation: &BridgeOperation) -> Result<(), Self::Error> {
        match operation {
            BridgeOperation::Create { bridge_id } => {
                let mut bridges = self.access.shared.bridges.lock_unpoisoned();
                if bridges.contains_key(bridge_id) {
                    return Err(AsteriskBackendError::BridgeConflict {
                        bridge_id: *bridge_id,
                    });
                }
                let bridge = self
                    .call_features
                    .create_bridge(*bridge_id)
                    .map_err(AsteriskBackendError::CallFeature)?;
                bridges.insert(*bridge_id, bridge);
                Ok(())
            }
            BridgeOperation::Destroy { bridge_id } => {
                let bridge = self
                    .access
                    .shared
                    .bridges
                    .lock_unpoisoned()
                    .remove(bridge_id)
                    .ok_or(AsteriskBackendError::BridgeUnavailable {
                        operation: "destroy bridge",
                        bridge_id: *bridge_id,
                    })?;
                bridge.destroy().map_err(AsteriskBackendError::CallFeature)
            }
            BridgeOperation::AddParticipant { bridge_id, call_id } => self
                .with_call_feature_channel("add bridge participant", *call_id, |channel| {
                    self.access
                        .shared
                        .bridges
                        .lock_unpoisoned()
                        .get_mut(bridge_id)
                        .ok_or(AsteriskBackendError::BridgeUnavailable {
                            operation: "add participant",
                            bridge_id: *bridge_id,
                        })?
                        .add(channel)
                        .map_err(AsteriskBackendError::CallFeature)
                }),
            BridgeOperation::RemoveParticipant { bridge_id, call_id } => self
                .with_call_feature_channel("remove bridge participant", *call_id, |channel| {
                    self.access
                        .shared
                        .bridges
                        .lock_unpoisoned()
                        .get_mut(bridge_id)
                        .ok_or(AsteriskBackendError::BridgeUnavailable {
                            operation: "remove participant",
                            bridge_id: *bridge_id,
                        })?
                        .remove(channel)
                        .map_err(AsteriskBackendError::CallFeature)
                }),
            BridgeOperation::MergeConsultation {
                bridge_id,
                original_call_id,
                consultation_call_id,
            } => {
                let mut bridge = self
                    .access
                    .shared
                    .bridges
                    .lock_unpoisoned()
                    .remove(bridge_id)
                    .ok_or(AsteriskBackendError::BridgeUnavailable {
                        operation: "merge conference consultation",
                        bridge_id: *bridge_id,
                    })?;
                let result = with_two_channels(
                    self.access,
                    *original_call_id,
                    *consultation_call_id,
                    |original, consultation| {
                        let original = unsafe { AsteriskChannel::from_raw(original.cast()) }
                            .map_err(|_| AsteriskBackendError::CallUnavailable {
                                operation: "merge conference consultation",
                                call_id: *original_call_id,
                            })?;
                        let consultation =
                            unsafe { AsteriskChannel::from_raw(consultation.cast()) }.map_err(
                                |_| AsteriskBackendError::CallUnavailable {
                                    operation: "merge conference consultation",
                                    call_id: *consultation_call_id,
                                },
                            )?;
                        bridge
                            .merge_consultation(&original, &consultation)
                            .map_err(AsteriskBackendError::CallFeature)
                    },
                )
                .unwrap_or(Err(AsteriskBackendError::CallUnavailable {
                    operation: "merge conference consultation",
                    call_id: *consultation_call_id,
                }));
                self.access
                    .shared
                    .bridges
                    .lock_unpoisoned()
                    .insert(*bridge_id, bridge);
                result
            }
            BridgeOperation::MergeCalls {
                bridge_id,
                call_ids,
            } => {
                let mut bridge = self
                    .access
                    .shared
                    .bridges
                    .lock_unpoisoned()
                    .remove(bridge_id)
                    .ok_or(AsteriskBackendError::BridgeUnavailable {
                        operation: "merge selected conference calls",
                        bridge_id: *bridge_id,
                    })?;
                let result = with_channels(self.access, call_ids, |channels| {
                    let channels: Result<Vec<_>, _> = channels
                        .iter()
                        .zip(call_ids)
                        .map(|(channel, call_id)| unsafe {
                            AsteriskChannel::from_raw(channel.cast()).map_err(|_| {
                                AsteriskBackendError::CallUnavailable {
                                    operation: "merge selected conference calls",
                                    call_id: *call_id,
                                }
                            })
                        })
                        .collect();
                    bridge
                        .merge_calls(&channels?)
                        .map_err(AsteriskBackendError::CallFeature)
                })
                .unwrap_or_else(|| {
                    Err(AsteriskBackendError::Failed {
                        operation: "merge selected conference calls",
                        calls: call_ids
                            .iter()
                            .map(|call_id| call_id.0.to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                    })
                });
                self.access
                    .shared
                    .bridges
                    .lock_unpoisoned()
                    .insert(*bridge_id, bridge);
                result
            }
            BridgeOperation::MergeParticipant { bridge_id, call_id } => {
                let mut bridge = self
                    .access
                    .shared
                    .bridges
                    .lock_unpoisoned()
                    .remove(bridge_id)
                    .ok_or(AsteriskBackendError::BridgeUnavailable {
                        operation: "merge conference participant",
                        bridge_id: *bridge_id,
                    })?;
                let result =
                    with_channel(self.access, *call_id, |channel| {
                        let channel = unsafe { AsteriskChannel::from_raw(channel.cast()) }
                            .map_err(|_| AsteriskBackendError::CallUnavailable {
                                operation: "merge conference participant",
                                call_id: *call_id,
                            })?;
                        bridge
                            .merge_participant(&channel)
                            .map_err(AsteriskBackendError::CallFeature)
                    })
                    .unwrap_or(Err(AsteriskBackendError::CallUnavailable {
                        operation: "merge conference participant",
                        call_id: *call_id,
                    }));
                self.access
                    .shared
                    .bridges
                    .lock_unpoisoned()
                    .insert(*bridge_id, bridge);
                result
            }
            BridgeOperation::SetParticipantMuted {
                bridge_id,
                participant_id: _,
                call_id,
                muted,
            } => self.with_call_feature_channel(
                "set conference participant mute",
                *call_id,
                |channel| {
                    self.access
                        .shared
                        .bridges
                        .lock_unpoisoned()
                        .get_mut(bridge_id)
                        .ok_or(AsteriskBackendError::BridgeUnavailable {
                            operation: "set participant mute",
                            bridge_id: *bridge_id,
                        })?
                        .set_participant_muted(channel, *muted)
                        .map_err(AsteriskBackendError::CallFeature)
                },
            ),
            BridgeOperation::SetParticipantMusicOnHold {
                bridge_id,
                participant_id: _,
                call_id,
                class,
                enabled,
            } => self.with_call_feature_channel(
                "set conference participant music on hold",
                *call_id,
                |channel| {
                    self.access
                        .shared
                        .bridges
                        .lock_unpoisoned()
                        .get_mut(bridge_id)
                        .ok_or(AsteriskBackendError::BridgeUnavailable {
                            operation: "set participant music on hold",
                            bridge_id: *bridge_id,
                        })?
                        .set_participant_music_on_hold(channel, class, *enabled)
                        .map_err(AsteriskBackendError::CallFeature)
                },
            ),
            BridgeOperation::RemoveConferenceParticipant {
                bridge_id,
                participant_id: _,
                call_id,
            } => self.with_call_feature_channel(
                "remove conference participant",
                *call_id,
                |channel| {
                    self.access
                        .shared
                        .bridges
                        .lock_unpoisoned()
                        .get_mut(bridge_id)
                        .ok_or(AsteriskBackendError::BridgeUnavailable {
                            operation: "remove conference participant",
                            bridge_id: *bridge_id,
                        })?
                        .remove_participant_and_hangup(channel)
                        .map_err(AsteriskBackendError::CallFeature)
                },
            ),
        }
    }

    fn barge(&self, operation: &BargeOperation) -> Result<(), Self::Error> {
        match operation {
            BargeOperation::Join {
                bridge_id,
                target_call_id,
                barger_call_id,
            } => {
                let exists = self
                    .access
                    .shared
                    .barge_bridges
                    .lock_unpoisoned()
                    .contains_key(bridge_id);
                if exists {
                    return self.with_call_feature_channel(
                        "add barge participant",
                        *barger_call_id,
                        |channel| {
                            self.access
                                .shared
                                .barge_bridges
                                .lock_unpoisoned()
                                .get_mut(bridge_id)
                                .ok_or(AsteriskBackendError::BridgeUnavailable {
                                    operation: "add barge participant",
                                    bridge_id: *bridge_id,
                                })?
                                .add(channel)
                                .map_err(AsteriskBackendError::CallFeature)
                        },
                    );
                }

                let mut bridge = self.with_call_feature_channel(
                    "acquire barge bridge",
                    *target_call_id,
                    |channel| {
                        self.call_features
                            .acquire_barge_bridge(*bridge_id, channel)
                            .map_err(AsteriskBackendError::CallFeature)
                    },
                )?;
                if let Err(error) = self.with_call_feature_channel(
                    "add barge participant",
                    *barger_call_id,
                    |channel| {
                        bridge
                            .add(channel)
                            .map_err(AsteriskBackendError::CallFeature)
                    },
                ) {
                    let _ = bridge.release();
                    return Err(error);
                }
                let replaced = self
                    .access
                    .shared
                    .barge_bridges
                    .lock_unpoisoned()
                    .insert(*bridge_id, bridge);
                if replaced.is_some() {
                    return Err(AsteriskBackendError::BridgeConflict {
                        bridge_id: *bridge_id,
                    });
                }
                Ok(())
            }
            BargeOperation::Leave {
                bridge_id,
                barger_call_id,
                last_participant,
            } => {
                let removal = self.with_call_feature_channel(
                    "remove barge participant",
                    *barger_call_id,
                    |channel| {
                        self.access
                            .shared
                            .barge_bridges
                            .lock_unpoisoned()
                            .get_mut(bridge_id)
                            .ok_or(AsteriskBackendError::BridgeUnavailable {
                                operation: "remove barge participant",
                                bridge_id: *bridge_id,
                            })?
                            .remove(channel)
                            .map_err(AsteriskBackendError::CallFeature)
                    },
                );
                let release = if *last_participant {
                    self.access
                        .shared
                        .barge_bridges
                        .lock_unpoisoned()
                        .remove(bridge_id)
                        .ok_or(AsteriskBackendError::BridgeUnavailable {
                            operation: "release barge bridge",
                            bridge_id: *bridge_id,
                        })?
                        .release()
                        .map_err(AsteriskBackendError::CallFeature)
                } else {
                    Ok(())
                };
                removal.and(release)
            }
        }
    }

    fn announce(&self, operation: &ConferenceAnnouncementOperation) -> Result<(), Self::Error> {
        play_conference_announcement(self.access, operation)
    }
}
