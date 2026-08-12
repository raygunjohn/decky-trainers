import { useState } from "react";
import { ConfirmModal, TextField } from "@decky/ui";

export interface SetValueModalProps {
  title: string;
  initialValue: string;
  onConfirm: (value: string) => void;
  closeModal?: () => void;
}

/**
 * Editing happens in a modal rather than inline: the Steam on-screen keyboard
 * and the panel's 2s poll would otherwise fight over the field.
 */
export function SetValueModal({
  title,
  initialValue,
  onConfirm,
  closeModal,
}: SetValueModalProps) {
  const [value, setValue] = useState(initialValue);

  return (
    <ConfirmModal
      strTitle={title}
      strOKButtonText="Set"
      closeModal={closeModal}
      onOK={() => onConfirm(value)}
    >
      <TextField
        focusOnMount
        label="Value"
        value={value}
        onChange={(e) => setValue(e.target.value)}
      />
    </ConfirmModal>
  );
}
